// sentiric-b2bua-service/src/sip/handlers/media.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use sentiric_sip_core::sdp::SdpBuilder; // Core'dan gelen Builder
use tokio::time::{timeout, Duration};

pub struct MediaManager {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl MediaManager {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    pub fn extract_port_from_sdp(&self, body: &[u8]) -> Option<u16> {
        let sdp_text = String::from_utf8_lossy(body);
        for line in sdp_text.lines() {
            if line.starts_with("m=audio ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() > 1 { return parts[1].parse().ok(); }
            }
        }
        None
    }

    pub async fn allocate_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut media_client = { let guard = self.clients.lock().await; guard.media.clone() }; 
        let mut req = Request::new(AllocatePortRequest { call_id: call_id.to_string() });
        req.metadata_mut().insert("x-trace-id", call_id.parse().unwrap_or_else(|_| "unknown".parse().unwrap()));
        let res = timeout(Duration::from_secs(3), media_client.allocate_port(req)).await?;
        Ok(res?.into_inner().rtp_port)
    }

    pub async fn release_port(&self, port: u32) {
        let mut media_client = { let guard = self.clients.lock().await; guard.media.clone() };
        tokio::spawn(async move { let _ = media_client.release_port(ReleasePortRequest { rtp_port: port }).await; });
    }

    /// [v1.4.4 GÜNCELLEMESİ]: Explicit Ptime Configuration
    /// Telekom standartlarına uyum için 20ms paketleme zorlanıyor.
    pub fn generate_sdp(&self, rtp_port: u32) -> Vec<u8> {
        SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            // [FIX 1] Öncelik PCMU (0) yapıldı. Media Service default PCMU kullanıyor.
            // Bu sayede sinyalleşme ve medya aynı dili konuşacak.
            .add_codec(0, "PCMU", 8000, None)
            
            // Yedekler
            .add_codec(18, "G729", 8000, Some("annexb=no"))
            .add_codec(8, "PCMA", 8000, None) // CIZIRTILI
            .add_codec(101, "telephone-event", 8000, Some("0-16"))
            
            // Standart Ptime
            .with_ptime(20)
            
            // [FIX 2] RTCP Kapalı.
            // NAT arkasındaki port eşleşme riskini sıfıra indirmek için.
            // Sadece tek bir port (RTP) üzerinden iletişim kurulsun.
            .with_rtcp(false)
            
            .build()
            .as_bytes()
            .to_vec()
    }
}