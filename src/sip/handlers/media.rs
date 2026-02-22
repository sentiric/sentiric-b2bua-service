// sentiric-b2bua-service/src/sip/handlers/media.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use sentiric_sip_core::sdp::SdpBuilder;
use sentiric_rtp_core::AudioProfile; 
use tokio::time::{timeout, Duration};
use std::str::FromStr; // [FIX]: EKLENDİ

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
        
        // [FIX]: FromStr sayesinde artık derlenecek
        if let Ok(val) = tonic::metadata::MetadataValue::from_str(call_id) {
            req.metadata_mut().insert("x-trace-id", val);
        }
        
        let res = timeout(Duration::from_secs(3), media_client.allocate_port(req)).await??;
        Ok(res.into_inner().rtp_port)
    }

    pub async fn release_port(&self, port: u32, call_id: &str) {
        let mut media_client = { let guard = self.clients.lock().await; guard.media.clone() };
        let mut req = Request::new(ReleasePortRequest { rtp_port: port });
        
        // [FIX]: FromStr sayesinde artık derlenecek
        if let Ok(val) = tonic::metadata::MetadataValue::from_str(call_id) {
            req.metadata_mut().insert("x-trace-id", val);
        }

        tokio::spawn(async move { 
            let _ = media_client.release_port(req).await; 
        });
    }

    pub fn generate_sdp(&self, rtp_port: u32) -> Vec<u8> {
        let profile = AudioProfile::default();
        
        let mut builder = SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_ptime(profile.ptime)
            .with_rtcp(false); 

        for codec_conf in profile.codecs {
            builder = builder.add_codec(
                codec_conf.payload_type, 
                codec_conf.name, 
                codec_conf.rate, 
                codec_conf.fmtp.as_deref()
            );
        }
        
        builder.build().as_bytes().to_vec()
    }
}