//! Sequencer-specific API options and initialization.

use super::{
    data_source::SequencerDataSource, endpoints, fs, sql, update::update_loop, AppState, Consensus,
    NodeIndex, SequencerNode,
};
use crate::{network, persistence};
use async_std::{
    sync::{Arc, RwLock},
    task::spawn,
};
use clap::Parser;
use futures::future::{BoxFuture, TryFutureExt};
use hotshot_query_service::{
    data_source::{ExtensibleDataSource, MetricsDataSource},
    status::{self, UpdateStatusData},
    Error,
};
use hotshot_types::traits::metrics::{Metrics, NoMetrics};
use tide_disco::App;

#[derive(Clone, Debug)]
pub struct Options {
    pub http: Http,
    pub query: Option<Query>,
    pub submit: Option<Submit>,
    pub status: Option<Status>,
    pub storage_fs: Option<persistence::fs::Options>,
    pub storage_sql: Option<persistence::sql::Options>,
}

impl From<Http> for Options {
    fn from(http: Http) -> Self {
        Self {
            http,
            query: None,
            submit: None,
            status: None,
            storage_fs: None,
            storage_sql: None,
        }
    }
}

impl Options {
    /// Add a query API module backed by a Postgres database.
    pub fn query_sql(mut self, query: Query, storage: persistence::sql::Options) -> Self {
        self.query = Some(query);
        self.storage_sql = Some(storage);
        self
    }

    /// Add a query API module backed by the file system.
    pub fn query_fs(mut self, query: Query, storage: persistence::fs::Options) -> Self {
        self.query = Some(query);
        self.storage_fs = Some(storage);
        self
    }

    /// Add a submit API module.
    pub fn submit(mut self, opt: Submit) -> Self {
        self.submit = Some(opt);
        self
    }

    /// Add a status API module.
    pub fn status(mut self, opt: Status) -> Self {
        self.status = Some(opt);
        self
    }

    /// Whether these options will run the query API.
    pub fn has_query_module(&self) -> bool {
        self.query.is_some() && (self.storage_fs.is_some() || self.storage_sql.is_some())
    }

    /// Start the server.
    ///
    /// The function `init_handle` is used to create a consensus handle from a metrics object. The
    /// metrics object is created from the API data source, so that consensus will populuate metrics
    /// that can then be read and served by the API.
    pub async fn serve<N, F>(mut self, init_handle: F) -> anyhow::Result<SequencerNode<N>>
    where
        N: network::Type,
        F: FnOnce(Box<dyn Metrics>) -> BoxFuture<'static, (Consensus<N>, NodeIndex)>,
    {
        // The server state type depends on whether we are running a query or status API or not, so
        // we handle the two cases differently.
        let node = if let Some(opt) = self.storage_sql.take() {
            init_with_query_module::<N, sql::DataSource>(self, opt, init_handle).await?
        } else if let Some(opt) = self.storage_fs.take() {
            init_with_query_module::<N, fs::DataSource>(self, opt, init_handle).await?
        } else if self.status.is_some() {
            // If a status API is requested but no availability API, we use the `MetricsDataSource`,
            // which allows us to run the status API with no persistent storage.
            let ds = MetricsDataSource::default();
            let (handle, node_index) = init_handle(ds.populate_metrics()).await;
            let mut app = App::<_, Error>::with_state(Arc::new(RwLock::new(
                ExtensibleDataSource::new(ds, handle.clone()),
            )));

            // Initialize status API.
            let status_api = status::define_api(&Default::default())?;
            app.register_module("status", status_api)?;

            // Initialize submit API
            if self.submit.is_some() {
                let submit_api = endpoints::submit()?;
                app.register_module("submit", submit_api)?;
            }

            SequencerNode {
                handle,
                node_index,
                update_task: spawn(
                    app.serve(format!("0.0.0.0:{}", self.http.port))
                        .map_err(anyhow::Error::from),
                ),
            }
        } else {
            // If no status or availability API is requested, we don't need metrics or a query
            // service data source. The only app state is the HotShot handle, which we use to submit
            // transactions.
            let (handle, node_index) = init_handle(Box::new(NoMetrics)).await;
            let mut app = App::<_, Error>::with_state(RwLock::new(handle.clone()));

            // Initialize submit API
            if self.submit.is_some() {
                let submit_api = endpoints::submit::<N, RwLock<Consensus<N>>>()?;
                app.register_module("submit", submit_api)?;
            }

            SequencerNode {
                handle,
                node_index,
                update_task: spawn(
                    app.serve(format!("0.0.0.0:{}", self.http.port))
                        .map_err(anyhow::Error::from),
                ),
            }
        };

        // Start consensus.
        node.handle.hotshot.start_consensus().await;
        Ok(node)
    }
}

/// The minimal HTTP API.
///
/// The API automatically includes health and version endpoints. Additional API modules can be
/// added by including the query-api or submit-api modules.
#[derive(Parser, Clone, Debug)]
pub struct Http {
    /// Port that the HTTP API will use.
    #[clap(long, env = "ESPRESSO_SEQUENCER_API_PORT")]
    pub port: u16,
}

/// Options for the submission API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Submit;

/// Options for the status API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Status;

/// Options for the query API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Query;

async fn init_with_query_module<N, D>(
    opt: Options,
    mod_opt: D::Options,
    init_handle: impl FnOnce(Box<dyn Metrics>) -> BoxFuture<'static, (Consensus<N>, NodeIndex)>,
) -> anyhow::Result<SequencerNode<N>>
where
    N: network::Type,
    D: SequencerDataSource + Send + Sync + 'static,
{
    type State<N, D> = Arc<RwLock<AppState<N, D>>>;

    let ds = D::create(mod_opt, false).await?;
    let metrics = ds.populate_metrics();

    // Start up handle
    let (mut handle, node_index) = init_handle(metrics).await;

    // Get an event stream from the handle to use for populating the query data with
    // consensus events.
    //
    // We must do this _before_ starting consensus on the handle, otherwise we could miss
    // the first events emitted by consensus.
    let events = handle.get_event_stream(Default::default()).await.0;

    let state: State<N, D> = Arc::new(RwLock::new(ExtensibleDataSource::new(ds, handle.clone())));
    let mut app = App::<_, Error>::with_state(state.clone());

    // Initialize submit API
    if opt.submit.is_some() {
        let submit_api = endpoints::submit::<N, State<N, D>>()?;
        app.register_module("submit", submit_api)?;
    }

    // Initialize status API
    if opt.status.is_some() {
        let status_api = status::define_api::<State<N, D>>(&Default::default())?;
        app.register_module("status", status_api)?;
    }

    // Initialize availability API
    let availability_api = endpoints::availability::<N, D>()?;
    app.register_module("availability", availability_api)?;

    let update_task = spawn(async move {
        futures::join!(
            app.serve(format!("0.0.0.0:{}", opt.http.port))
                .map_err(anyhow::Error::from),
            update_loop(state, events),
        )
        .0
    });

    Ok(SequencerNode {
        handle,
        node_index,
        update_task,
    })
}