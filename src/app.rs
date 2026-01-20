// sentiric-b2bua-service/src/app.rs
use crate::config::AppConfig;
use crate::grpc::service::MyB2BuaService;
use crate::grpc::client::InternalClients;
use crate::tls::load_server_tls_config;
use crate::sip::engine::B2BuaEngine;
use crate::sip::state::new_store;
use crate::sip::server::SipServer;
use anyhow::{Context, Result};
use sentiric_contracts::sentiric::sip::v1::b2bua_service_server::B2buaServiceServer;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::transport::Server as GrpcServer; 
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server as HttpServer, StatusCode,
};
use sentiric_sip_core::SipTransport;

pub struct App {
    config: Arc<AppConfig>,
}

async fn handle_http_request(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"status":"ok", "service": "b2bua-service"}"#))
        .unwrap())
}

impl App {
    pub async fn bootstrap() -> Result<Self> {
        dotenvy::dotenv().ok();
        let config = Arc::new(AppConfig::load_from_env().context("Konfigürasyon yüklenemedi")?);

        let rust_log_env = env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
        let env_filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&rust_log_env))?;
        let subscriber = Registry::default().with(env_filter);
        
        if config.env == "development" {
            subscriber.with(fmt::layer().with_target(true).with_line_number(true)).init();
        } else {
            subscriber.with(fmt::layer().json().with_current_span(true).with_span_list(true)).init();
        }

        info!(
            service_name = "sentiric-b2bua-service",
            version = %config.service_version,
            profile = %config.env,
            "🚀 Servis başlatılıyor..."
        );
        Ok(Self { config })
    }

    pub async fn run(self) -> Result<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let (sip_shutdown_tx, sip_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();

        // 1. Clients
        let clients = Arc::new(Mutex::new(InternalClients::connect(&self.config).await?));
        let calls = new_store();

        // 2. Transport & Engine
        let bind_addr = format!("{}:{}", self.config.sip_bind_ip, self.config.sip_port);
        let transport = Arc::new(SipTransport::new(&bind_addr).await?);
        
        let engine = Arc::new(B2BuaEngine::new(self.config.clone(), clients, calls, transport.clone()));

        // 3. SIP Server
        let sip_server = SipServer::new(engine.clone(), transport);
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        // 4. gRPC Server
        let grpc_config = self.config.clone();
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config).await.expect("TLS hatası");
            let grpc_service = MyB2BuaService::new(engine); 
            
            info!(address = %grpc_config.grpc_listen_addr, "gRPC sunucusu başlatılıyor...");
            
            GrpcServer::builder()
                .tls_config(tls_config).expect("TLS yapılandırma hatası")
                .add_service(B2buaServiceServer::new(grpc_service))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    shutdown_rx.recv().await;
                })
                .await
                .context("gRPC sunucusu çöktü")
        });

        // 5. HTTP Server
        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });
            let server = HttpServer::bind(&addr).serve(make_svc).with_graceful_shutdown(async {
                http_shutdown_rx.await.ok();
            });
            info!(address = %addr, "HTTP sağlık kontrolü aktif.");
            if let Err(e) = server.await { error!(error = %e, "HTTP sunucusu hatası"); }
        });

        let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl+C hatası"); };
        
        tokio::select! {
            res = grpc_server_handle => { if let Err(e) = res? { error!("gRPC Error: {}", e); } },
            res = http_server_handle => { error!("HTTP Server durdu"); },
            res = sip_handle => { error!("SIP Server durdu"); },
            _ = ctrl_c => { warn!("Kapatma sinyali alındı."); },
        }

        let _ = shutdown_tx.send(()).await;
        let _ = sip_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());
        
        info!("Servis durduruldu.");
        Ok(())
    }
}