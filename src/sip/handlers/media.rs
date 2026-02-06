// sentiric-b2bua-service/src/sip/handlers/media.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, warn, instrument, debug}; // debug eklendi
use tonic::Request;
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest, PlayAudioRequest};
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

    #[instrument(skip(self), fields(call_id = %call_id))]
    pub async fn allocate_port(&self, call_id: &str) -> anyhow::Result<u32> {
        // [FIX] Mutex scope'unu küçültüyoruz. Sadece client'ı klonlamak için kilitliyoruz.
        // gRPC çağrısı sırasında kilit tutulmamalı!
        let media_client = {
            let clients_guard = self.clients.lock().await;
            clients_guard.media.clone() 
        };
        // Not: InternalClients içindeki 'media' alanı 'MediaServiceClient<Channel>' tipindedir ve bu tip ucuzca klonlanabilir (Buffer gibidir).

        // Artık kilit serbest. Şimdi isteği hazırlayalım.
        let mut req = Request::new(AllocatePortRequest { call_id: call_id.to_string() });
        req.metadata_mut().insert("x-trace-id", call_id.parse().unwrap_or_else(|_| "unknown".parse().unwrap()));

        info!("⏳ Media Service'den port isteniyor (Timeout: 3s)...");

        // Clone'lanmış client (mut) üzerinden çağrı yapıyoruz.
        let mut client_clone = media_client; 
        
        let response_result = timeout(Duration::from_secs(3), client_clone.allocate_port(req)).await;

        match response_result {
            Ok(rpc_result) => {
                match rpc_result {
                    Ok(resp) => {
                        let port = resp.into_inner().rtp_port;
                        info!("✅ Media Port Allocated: {}", port);
                        Ok(port)
                    },
                    Err(e) => {
                        error!("❌ Media Service gRPC Hatası: {}", e);
                        Err(anyhow::anyhow!("Media Service Error: {}", e))
                    }
                }
            },
            Err(_) => {
                error!("🔥 Media Service TIMEOUT! (3s doldu, yanıt yok)");
                Err(anyhow::anyhow!("Media Service Allocation Timeout"))
            }
        }
    }

    pub async fn release_port(&self, port: u32) {
        // Burada da aynı mantık: Kilidi al, client'ı al, kilidi bırak, işi yap.
        let mut media_client = {
            let clients_guard = self.clients.lock().await;
            clients_guard.media.clone()
        };
        
        // Background task olarak ateşle ve unut, ana akışı bloklama.
        tokio::spawn(async move {
            if let Err(e) = media_client.release_port(Request::new(ReleasePortRequest { rtp_port: port })).await {
                warn!("⚠️ Port {} serbest bırakılamadı: {}", port, e);
            } else {
                info!("♻️ Media Port Released: {}", port);
            }
        });
    }

    pub async fn trigger_hole_punching(&self, rtp_port: u32, target_addr: String) {
        let mut media_client = {
            let clients_guard = self.clients.lock().await;
            clients_guard.media.clone()
        };

        tokio::spawn(async move {
            let req = Request::new(PlayAudioRequest { 
                audio_uri: "file://audio/tr/system/nat_warmer.wav".to_string(), 
                server_rtp_port: rtp_port, 
                rtp_target_addr: target_addr.clone() 
            });

            match media_client.play_audio(req).await {
                Ok(_) => info!("🔨 Hole Punching tetiklendi -> {}", target_addr),
                Err(e) => warn!("⚠️ Hole Punching başarısız: {}", e),
            }
        });
    }

    pub fn generate_sdp(&self, rtp_port: u32) -> Vec<u8> {
        SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs()
            .build()
            .as_bytes()
            .to_vec()
    }
}