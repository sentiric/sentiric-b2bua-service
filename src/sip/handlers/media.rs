// sentiric-b2bua-service/src/sip/handlers/media.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use sentiric_sip_core::sdp::SdpBuilder;
use tokio::time::{timeout, Duration};

pub struct MediaManager {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl MediaManager {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    /// [TELEKOM KRİTİK]: Ham SDP içeriğinden ses portunu ayıklar.
    /// SBC ile Media Service arasındaki 'Sticky Port' eşleşmesi için kullanılır.
    pub fn extract_port_from_sdp(&self, body: &[u8]) -> Option<u16> {
        let sdp_text = String::from_utf8_lossy(body);
        for line in sdp_text.lines() {
            if line.starts_with("m=audio ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() > 1 {
                    return parts[1].parse().ok();
                }
            }
        }
        None
    }

    pub async fn allocate_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut media_client = {
            let clients_guard = self.clients.lock().await;
            clients_guard.media.clone()
        }; 

        let mut req = Request::new(AllocatePortRequest { call_id: call_id.to_string() });
        // Trace ID'yi metadata'ya ekle
        req.metadata_mut().insert("x-trace-id", call_id.parse().unwrap_or_else(|_| "unknown".parse().unwrap()));

        let res = timeout(Duration::from_secs(3), media_client.allocate_port(req)).await?;
        Ok(res?.into_inner().rtp_port)
    }

    pub async fn release_port(&self, port: u32) {
        let mut media_client = {
            let clients_guard = self.clients.lock().await;
            clients_guard.media.clone()
        };
        tokio::spawn(async move {
            let _ = media_client.release_port(ReleasePortRequest { rtp_port: port }).await;
        });
    }

    pub fn generate_sdp(&self, rtp_port: u32) -> Vec<u8> {
        SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs() // G.729, PCMA, PCMU desteği
            .build()
            .as_bytes()
            .to_vec()
    }
}