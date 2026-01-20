// sentiric-b2bua-service/src/sip/server.rs

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use sentiric_sip_core::{SipTransport, parser};
use crate::sip::engine::B2BuaEngine;

pub struct SipServer {
    engine: Arc<B2BuaEngine>,
    transport: Arc<SipTransport>,
}

impl SipServer {
    pub fn new(engine: Arc<B2BuaEngine>, transport: Arc<SipTransport>) -> Self {
        Self { engine, transport }
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!("📡 B2BUA SIP Listener aktif.");

        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("🛑 SIP Server kapatılıyor...");
                    break;
                }
                
                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, _src_addr)) => {
                            let data = &buf[..len];
                            match parser::parse(data) {
                                Ok(packet) => {
                                    let engine = self.engine.clone();
                                    tokio::spawn(async move {
                                        engine.handle_packet(packet).await;
                                    });
                                },
                                Err(e) => {
                                    warn!("SIP parse hatası: {}", e);
                                }
                            }
                        },
                        Err(e) => error!("UDP Error: {}", e),
                    }
                }
            }
        }
    }
}