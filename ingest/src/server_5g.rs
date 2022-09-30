use crate::{error::DecodeError, Error, EventId, Result};
use chrono::Utc;
use file_store::traits::MsgVerify;
use file_store::{file_sink, file_upload, FileType};
use futures_util::TryFutureExt;
use helium_crypto::PublicKey;
use helium_proto::services::poc_mobile::{
    self, CellHeartbeatIngestReportV1, CellHeartbeatReqV1, CellHeartbeatRespV1,
    SpeedtestIngestReportV1, SpeedtestReqV1, SpeedtestRespV1,
};
use std::env;
use std::{net::SocketAddr, path::Path, str::FromStr};
use tonic::{metadata::MetadataValue, transport, Request, Response, Status};

pub type GrpcResult<T> = std::result::Result<Response<T>, Status>;

pub struct GrpcServer {
    heartbeat_req_tx: file_sink::MessageSender,
    speedtest_req_tx: file_sink::MessageSender,
    heartbeat_report_tx: file_sink::MessageSender,
    speedtest_report_tx: file_sink::MessageSender,
}

#[tonic::async_trait]
impl poc_mobile::PocMobile for GrpcServer {
    async fn submit_speedtest(
        &self,
        request: Request<SpeedtestReqV1>,
    ) -> GrpcResult<SpeedtestRespV1> {
        let timestamp = Utc::now().timestamp_millis() as u64;
        let event = request.into_inner();
        let public_key = PublicKey::try_from(event.pub_key.as_ref())
            .map_err(|_| Status::invalid_argument("invalid public key"))?;
        event
            .verify(&public_key)
            .map_err(|_| Status::invalid_argument("invalid signature"))?;
        // Encode event digest, encode and return as the id
        let event_id = EventId::from(&event);

        let report = SpeedtestIngestReportV1 {
            report: Some(event.clone()),
            received_timestamp: timestamp,
        };

        match file_sink::write(&self.speedtest_req_tx, event).await {
            Ok(_) => (),
            Err(err) => tracing::error!("failed to store speedtest: {err:?}"),
        }

        match file_sink::write(&self.speedtest_report_tx, report).await {
            Ok(_) => (),
            Err(err) => tracing::error!("failed to store speedtest: {err:?}"),
        }

        metrics::increment_counter!("ingest_server_speedtest_count");
        Ok(Response::new(event_id.into()))
    }

    async fn submit_cell_heartbeat(
        &self,
        request: Request<CellHeartbeatReqV1>,
    ) -> GrpcResult<CellHeartbeatRespV1> {
        let timestamp = Utc::now().timestamp_millis() as u64;
        let event = request.into_inner();
        let public_key = PublicKey::try_from(event.pub_key.as_slice())
            .map_err(|_| Status::invalid_argument("invalid public key"))?;
        event
            .verify(&public_key)
            .map_err(|_| Status::invalid_argument("invalid signature"))?;
        let event_id = EventId::from(&event);

        let report = CellHeartbeatIngestReportV1 {
            report: Some(event.clone()),
            received_timestamp: timestamp,
        };

        match file_sink::write(&self.heartbeat_req_tx, event).await {
            Ok(_) => (),
            Err(err) => tracing::error!("failed to store heartbeat: {err:?}"),
        }

        match file_sink::write(&self.heartbeat_report_tx, report).await {
            Ok(_) => (),
            Err(err) => tracing::error!("failed to store heartbeat: {err:?}"),
        }

        metrics::increment_counter!("ingest_server_heartbeat_count");
        // Encode event digest, encode and return as the id
        Ok(Response::new(event_id.into()))
    }
}

pub async fn grpc_server(shutdown: triggered::Listener, server_mode: String) -> Result {
    let grpc_addr: SocketAddr = env::var("GRPC_SOCKET_ADDR")
        .map_or_else(
            |_| SocketAddr::from_str("0.0.0.0:9081"),
            |str| SocketAddr::from_str(&str),
        )
        .map_err(DecodeError::from)?;

    // Initialize uploader
    let (file_upload_tx, file_upload_rx) = file_upload::message_channel();
    let file_upload =
        file_upload::FileUpload::from_env_with_prefix("INGESTOR", file_upload_rx).await?;

    let store_path =
        std::env::var("INGEST_STORE").unwrap_or_else(|_| String::from("/var/data/ingestor"));
    let store_base_path = Path::new(&store_path);

    let (heartbeat_req_tx, heartbeat_req_rx) = file_sink::message_channel(50);
    let mut heartbeat_req_sink =
        file_sink::FileSinkBuilder::new(FileType::CellHeartbeat, store_base_path, heartbeat_req_rx)
            .deposits(Some(file_upload_tx.clone()))
            .create()
            .await?;

    let (heartbeat_report_tx, heartbeat_report_rx) = file_sink::message_channel(50);
    let mut heartbeat_report_sink = file_sink::FileSinkBuilder::new(
        FileType::CellHeartbeatIngestReport,
        store_base_path,
        heartbeat_report_rx,
    )
    .deposits(Some(file_upload_tx.clone()))
    .create()
    .await?;

    // speedtests
    let (speedtest_req_tx, speedtest_req_rx) = file_sink::message_channel(50);
    let mut speedtest_req_sink =
        file_sink::FileSinkBuilder::new(FileType::CellSpeedtest, store_base_path, speedtest_req_rx)
            .deposits(Some(file_upload_tx.clone()))
            .create()
            .await?;

    let (speedtest_report_tx, speedtest_report_rx) = file_sink::message_channel(50);
    let mut speedtest_report_sink = file_sink::FileSinkBuilder::new(
        FileType::CellSpeedtestIngestReport,
        store_base_path,
        speedtest_report_rx,
    )
    .deposits(Some(file_upload_tx.clone()))
    .create()
    .await?;

    let grpc_server = GrpcServer {
        speedtest_req_tx,
        heartbeat_req_tx,
        speedtest_report_tx,
        heartbeat_report_tx,
    };
    let api_token = std::env::var("API_TOKEN").map(|token| {
        format!("Bearer {}", token)
            .parse::<MetadataValue<_>>()
            .unwrap()
    })?;

    tracing::info!(
        "grpc listening on {} and server mode {}",
        grpc_addr,
        server_mode
    );

    //TODO start a service with either the poc mobile or poc lora endpoints only - not both
    //     use _server_mode (set above ) to decide
    let server = transport::Server::builder()
        .layer(poc_metrics::ActiveRequestsLayer::new(
            "ingest_server_grpc_connection_count",
        ))
        .add_service(poc_mobile::Server::with_interceptor(
            grpc_server,
            move |req: Request<()>| match req.metadata().get("authorization") {
                Some(t) if api_token == t => Ok(req),
                _ => Err(Status::unauthenticated("No valid auth token")),
            },
        ))
        .serve_with_shutdown(grpc_addr, shutdown.clone())
        .map_err(Error::from);

    tokio::try_join!(
        server,
        heartbeat_req_sink.run(&shutdown).map_err(Error::from),
        speedtest_req_sink.run(&shutdown).map_err(Error::from),
        heartbeat_report_sink.run(&shutdown).map_err(Error::from),
        speedtest_report_sink.run(&shutdown).map_err(Error::from),
        file_upload.run(&shutdown).map_err(Error::from),
    )
    .map(|_| ())
}