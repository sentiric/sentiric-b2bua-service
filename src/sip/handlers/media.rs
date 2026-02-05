// sentiric-b2bua-service/src/sip/handlers/media.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use tonic::Request;
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest, PlayAudioRequest};
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use sentiric_sip_core::sdp::SdpBuilder;

pub struct MediaManager {
    clients: Arc<Mutex<InternalClients>>,
    config: Arc<AppConfig>,
}

impl MediaManager {
    pub fn new(clients: Arc<Mutex<InternalClients>>, config: Arc<AppConfig>) -> Self {
        Self { clients, config }
    }

    pub async fn allocate_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut clients = self.clients.lock().await;
        let resp = clients.media.allocate_port(Request::new(AllocatePortRequest { call_id: call_id.to_string() })).await?.into_inner();
        info!("Media Port Allocated: {}", resp.rtp_port);
        Ok(resp.rtp_port)
    }

    pub async fn release_port(&self, port: u32) {
        let mut clients = self.clients.lock().await;
        let _ = clients.media.release_port(Request::new(ReleasePortRequest { rtp_port: port })).await;
        info!("Media Port Released: {}", port);
    }

    pub async fn trigger_hole_punching(&self, rtp_port: u32, target_addr: String) {
        let mut clients = self.clients.lock().await;
        let _ = clients.media.play_audio(Request::new(PlayAudioRequest { 
            audio_uri: "file://audio/tr/system/nat_warmer.wav".to_string(), 
            server_rtp_port: rtp_port, 
            rtp_target_addr: target_addr 
        })).await;
    }

    pub fn generate_sdp(&self, rtp_port: u32) -> Vec<u8> {
        // sip-core içindeki SdpBuilder kullanılıyor, rtp-core'a gerek yok.
        SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs()
            .build()
            .as_bytes()
            .to_vec()
    }
}