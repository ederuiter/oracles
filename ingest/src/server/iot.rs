use crate::{
    server::{GrpcResult, VerifyResult},
    Error, EventId, Result, Settings,
};
use chrono::Utc;
use file_store::traits::MsgVerify;
use file_store::{file_sink, file_sink_write, file_upload, FileType};
use futures_util::TryFutureExt;
use helium_crypto::{Network, PublicKey};
use helium_proto::services::poc_lora::{
    self, LoraBeaconIngestReportV1, LoraBeaconReportReqV1, LoraBeaconReportRespV1,
    LoraWitnessIngestReportV1, LoraWitnessReportReqV1, LoraWitnessReportRespV1,
};
use std::path::Path;
use tonic::{transport, Request, Response, Status};

struct Server {
    lora_beacon_report_tx: file_sink::MessageSender,
    lora_witness_report_tx: file_sink::MessageSender,
    required_network: Network,
}

impl Server {
    fn decode_pub_key(&self, data: &[u8]) -> VerifyResult<PublicKey> {
        PublicKey::try_from(data).map_err(|_| Status::invalid_argument("invalid public key"))
    }

    fn verify_network(&self, public_key: PublicKey) -> VerifyResult<PublicKey> {
        (self.required_network == public_key.network)
            .then_some(public_key)
            .ok_or_else(|| Status::invalid_argument("invalid network"))
    }

    fn verify_signature(&self, pub_key: &PublicKey, event: impl MsgVerify) -> VerifyResult<()> {
        event
            .verify(pub_key)
            .map_err(|_| Status::invalid_argument("invalid signature"))
    }
}

#[tonic::async_trait]
impl poc_lora::PocLora for Server {
    async fn submit_lora_beacon(
        &self,
        request: Request<LoraBeaconReportReqV1>,
    ) -> GrpcResult<LoraBeaconReportRespV1> {
        let event = request.into_inner();

        self.decode_pub_key(event.pub_key.as_ref())
            .and_then(|pub_key| self.verify_network(pub_key))
            .and_then(|pub_key| self.verify_signature(&pub_key, event.clone()))?;

        let event_id = EventId::from(&event);
        let received_timestamp: u64 = Utc::now().timestamp_millis() as u64;
        let report = LoraBeaconIngestReportV1 {
            received_timestamp,
            report: Some(event),
        };
        let _ = file_sink_write!(
            "beacon_report",
            &self.lora_beacon_report_tx,
            report,
            format!("event_id:{:?}", event_id.to_string())
        )
        .await;
        // Encode event digest, encode and return as the id
        Ok(Response::new(event_id.into()))
    }

    async fn submit_lora_witness(
        &self,
        request: Request<LoraWitnessReportReqV1>,
    ) -> GrpcResult<LoraWitnessReportRespV1> {
        let event = request.into_inner();

        self.decode_pub_key(event.pub_key.as_ref())
            .and_then(|pub_key| self.verify_network(pub_key))
            .and_then(|pub_key| self.verify_signature(&pub_key, event.clone()))?;

        let event_id = EventId::from(&event);
        let received_timestamp: u64 = Utc::now().timestamp_millis() as u64;
        let report = LoraWitnessIngestReportV1 {
            received_timestamp,
            report: Some(event),
        };
        let _ = file_sink_write!(
            "witness_report",
            &self.lora_witness_report_tx,
            report,
            format!("event_id: {event_id}")
        )
        .await;
        // Encode event digest, encode and return as the id
        Ok(Response::new(event_id.into()))
    }
}

pub async fn run(shutdown: triggered::Listener, settings: &Settings) -> Result {
    let grpc_addr = settings.listen_addr()?;

    // Initialize uploader
    let (file_upload_tx, file_upload_rx) = file_upload::message_channel();
    let file_upload =
        file_upload::FileUpload::from_settings(&settings.output, file_upload_rx).await?;

    let store_base_path = Path::new(&settings.cache);

    // lora beacon reports
    let (lora_beacon_report_tx, lora_beacon_report_rx) = file_sink::message_channel(50);
    let mut lora_beacon_report_sink = file_sink::FileSinkBuilder::new(
        FileType::LoraBeaconIngestReport,
        store_base_path,
        lora_beacon_report_rx,
    )
    .deposits(Some(file_upload_tx.clone()))
    .create()
    .await?;

    // lora witness reports
    let (lora_witness_report_tx, lora_witness_report_rx) = file_sink::message_channel(50);
    let mut lora_witness_report_sink = file_sink::FileSinkBuilder::new(
        FileType::LoraWitnessIngestReport,
        store_base_path,
        lora_witness_report_rx,
    )
    .deposits(Some(file_upload_tx.clone()))
    .create()
    .await?;

    let grpc_server = Server {
        lora_beacon_report_tx,
        lora_witness_report_tx,
        required_network: settings.network,
    };

    tracing::info!(
        "grpc listening on {grpc_addr} and server mode {}",
        settings.mode
    );

    let server = transport::Server::builder()
        .layer(poc_metrics::request_layer!("ingest_server_lora_connection"))
        .add_service(poc_lora::Server::new(grpc_server))
        .serve_with_shutdown(grpc_addr, shutdown.clone())
        .map_err(Error::from);

    tokio::try_join!(
        server,
        lora_beacon_report_sink.run(&shutdown).map_err(Error::from),
        lora_witness_report_sink.run(&shutdown).map_err(Error::from),
        file_upload.run(&shutdown).map_err(Error::from),
    )
    .map(|_| ())
}