use std::{collections::HashMap, fmt::Debug, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_sol_types::eip712_domain;
use anyhow;
use axum::{
    async_trait,
    body::Body,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router, Server,
};
use eventuals::Eventual;
use reqwest::StatusCode;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::postgres::PgPoolOptions;
use thegraph::types::DeploymentId;
use thiserror::Error;

use crate::{
    prelude::{
        attestation_signers, dispute_manager, escrow_accounts, indexer_allocations,
        AttestationSigner, SubgraphClient,
    },
    tap_manager::TapManager,
};

use super::{request_handler::request_handler, IndexerServiceConfig};

pub trait IsAttestable {
    fn is_attestable(&self) -> bool;
}

#[async_trait]
pub trait IndexerServiceImpl {
    type Error: std::error::Error;
    type Request: DeserializeOwned + Send + Debug + Serialize;
    type Response: IntoResponse + Serialize + IsAttestable;
    type State: Send + Sync;

    async fn process_request(
        &self,
        manifest_id: DeploymentId,
        request: Self::Request,
    ) -> Result<(Self::Request, Self::Response), Self::Error>;
}

#[derive(Debug, Error)]
pub enum IndexerServiceError<E>
where
    E: std::error::Error,
{
    #[error("No receipt provided with the request")]
    NoReceipt,
    #[error("Issues with provided receipt: {0}")]
    ReceiptError(anyhow::Error),
    #[error("Service is not ready yet, try again in a moment")]
    ServiceNotReady,
    #[error("No attestation signer found for allocation `{0}`")]
    NoSignerForAllocation(Address),
    #[error("No attestation signer found for manifest `{0}`")]
    NoSignerForManifest(DeploymentId),
    #[error("Invalid request body: {0}")]
    InvalidRequest(anyhow::Error),
    #[error("Error while processing the request: {0}")]
    ProcessingError(E),
    #[error("No receipt or free query auth token provided")]
    Unauthorized,
    #[error("Invalid free query auth token: {0}")]
    InvalidFreeQueryAuthToken(String),
    #[error("Failed to sign attestation")]
    FailedToSignAttestation,
    #[error("Failed to provide attestation")]
    FailedToProvideAttestation,
    #[error("Failed to provide response")]
    FailedToProvideResponse,
}

impl<E> From<&IndexerServiceError<E>> for StatusCode
where
    E: std::error::Error,
{
    fn from(err: &IndexerServiceError<E>) -> Self {
        use IndexerServiceError::*;

        match err {
            ServiceNotReady => StatusCode::SERVICE_UNAVAILABLE,

            NoReceipt => StatusCode::PAYMENT_REQUIRED,

            Unauthorized => StatusCode::UNAUTHORIZED,

            NoSignerForAllocation(_) => StatusCode::INTERNAL_SERVER_ERROR,
            NoSignerForManifest(_) => StatusCode::INTERNAL_SERVER_ERROR,
            FailedToSignAttestation => StatusCode::INTERNAL_SERVER_ERROR,
            FailedToProvideAttestation => StatusCode::INTERNAL_SERVER_ERROR,
            FailedToProvideResponse => StatusCode::INTERNAL_SERVER_ERROR,

            ReceiptError(_) => StatusCode::BAD_REQUEST,
            InvalidRequest(_) => StatusCode::BAD_REQUEST,
            InvalidFreeQueryAuthToken(_) => StatusCode::BAD_REQUEST,
            ProcessingError(_) => StatusCode::BAD_REQUEST,
        }
    }
}

// Tell axum how to convert `RpcError` into a response.
impl<E> IntoResponse for IndexerServiceError<E>
where
    E: std::error::Error,
{
    fn into_response(self) -> Response {
        (StatusCode::from(&self), self.to_string()).into_response()
    }
}

pub struct IndexerServiceOptions<I>
where
    I: IndexerServiceImpl + Sync + Send + 'static,
{
    pub service_impl: I,
    pub config: IndexerServiceConfig,
    pub extra_routes: Router<Arc<IndexerServiceState<I>>, Body>,
}

pub struct IndexerServiceState<I>
where
    I: IndexerServiceImpl + Sync + Send + 'static,
{
    pub config: IndexerServiceConfig,
    pub attestation_signers: Eventual<HashMap<Address, AttestationSigner>>,
    pub tap_manager: TapManager,
    pub service_impl: Arc<I>,
}

pub struct IndexerService {}

impl IndexerService {
    pub async fn run<I>(options: IndexerServiceOptions<I>) -> Result<(), anyhow::Error>
    where
        I: IndexerServiceImpl + Sync + Send + 'static,
    {
        let network_subgraph = Box::leak(Box::new(SubgraphClient::new(
            "network-subgraph",
            &options.config.network_subgraph.query_url,
        )?));

        // Identify the dispute manager for the configured network
        let dispute_manager = dispute_manager(
            network_subgraph,
            options.config.graph_network.id,
            Duration::from_secs(3600),
        );

        // Monitor the indexer's own allocations
        let allocations = indexer_allocations(
            network_subgraph,
            options.config.indexer.indexer_address,
            options.config.graph_network.id,
            Duration::from_secs(options.config.network_subgraph.syncing_interval),
        );

        // Maintain an up-to-date set of attestation signers, one for each
        // allocation
        let attestation_signers = attestation_signers(
            allocations.clone(),
            options.config.indexer.operator_mnemonic.clone(),
            options.config.graph_network.id.into(),
            dispute_manager,
        );

        let escrow_subgraph = Box::leak(Box::new(SubgraphClient::new(
            "escrow-subgraph",
            &options.config.escrow_subgraph.query_url,
        )?));

        let escrow_accounts = escrow_accounts(
            escrow_subgraph,
            options.config.indexer.indexer_address,
            Duration::from_secs(options.config.escrow_subgraph.syncing_interval),
        );

        // Establish Database connection necessary for serving indexer management
        // requests with defined schema
        // Note: Typically, you'd call `sqlx::migrate!();` here to sync the models
        // which defaults to files in  "./migrations" to sync the database;
        // however, this can cause conflicts with the migrations run by indexer
        // agent. Hence we leave syncing and migrating entirely to the agent and
        // assume the models are up to date in the service.
        let database = PgPoolOptions::new()
            .max_connections(50)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&options.config.database.postgres_url)
            .await?;

        let tap_manager = TapManager::new(
            database,
            allocations,
            escrow_accounts,
            // TODO: arguments for eip712_domain should be a config
            eip712_domain! {
                name: "TapManager",
                version: "1",
                verifying_contract: options.config.indexer.indexer_address,
            },
        );

        let state = Arc::new(IndexerServiceState {
            config: options.config.clone(),
            attestation_signers,
            tap_manager,
            service_impl: Arc::new(options.service_impl),
        });

        let router = Router::new()
            .route("/", get("Service is up and running"))
            .route(
                PathBuf::from(options.config.server.url_prefix)
                    .join("manifests/:id")
                    .to_str()
                    .expect("Failed to set up `/manifest/:id` route"),
                post(request_handler::<I>),
            )
            .merge(options.extra_routes)
            .with_state(state);

        Ok(Server::bind(&options.config.server.host_and_port)
            .serve(router.into_make_service())
            .await?)
    }
}