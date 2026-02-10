// sentiric-b2bua-service/src/config.rs
use anyhow::{Context, Result};
use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub grpc_listen_addr: SocketAddr,
    pub http_listen_addr: SocketAddr,
    
    // SIP Network
    pub sip_bind_ip: String,
    pub sip_port: u16,
    
    // Dependencies (gRPC)
    pub media_service_url: String,
    pub proxy_service_url: String, 
    pub registrar_service_url: String,
    pub user_service_url: String,
    pub dialplan_service_url: String,
    
    // Dependencies (Infra)
    pub rabbitmq_url: String,
    pub redis_url: String,
    
    // SIP Routing
    pub proxy_sip_addr: String,
    
    // [GÜNCELLENDİ] Medya Hedefi
    pub sbc_sip_addr: String,
    pub sbc_public_ip: String, // SBC'nin dış IP'si
    
    // Identity
    pub public_ip: String, 
    pub sip_realm: String,
    
    pub env: String,
    pub rust_log: String,
    pub service_version: String,
    
    // Security
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self> {
        let grpc_port = env::var("B2BUA_SERVICE_GRPC_PORT").unwrap_or_else(|_| "13081".to_string());
        let http_port = env::var("B2BUA_SERVICE_HTTP_PORT").unwrap_or_else(|_| "13080".to_string());
        
        let sip_port_str = env::var("B2BUA_SERVICE_SIP_PORT").unwrap_or_else(|_| "13084".to_string());
        let sip_port = sip_port_str.parse::<u16>().context("Geçersiz SIP portu")?;

        let grpc_addr: SocketAddr = format!("[::]:{}", grpc_port).parse()?;
        let http_addr: SocketAddr = format!("[::]:{}", http_port).parse()?;
        
        let proxy_target = env::var("PROXY_SERVICE_SIP_TARGET")
            .unwrap_or_else(|_| "proxy-service:13074".to_string());

        let sbc_target = env::var("SBC_SERVICE_SIP_TARGET")
            .context("ZORUNLU: SBC_SERVICE_SIP_TARGET")?;
            
        // [YENİ] SBC'nin dış IP'sini de alıyoruz. Bu, B2BUA'nın kendisinin public_ip'si ile aynı olmalı.
        let sbc_public_ip = env::var("SBC_SERVICE_PUBLIC_IP")
            .context("ZORUNLU: SBC_SERVICE_PUBLIC_IP")?;

        Ok(AppConfig {
            grpc_listen_addr: grpc_addr,
            http_listen_addr: http_addr, 

            sip_bind_ip: "0.0.0.0".to_string(),
            sip_port,
            proxy_sip_addr: proxy_target,
            sbc_sip_addr: sbc_target,
            sbc_public_ip: sbc_public_ip.clone(), // Kendi public IP'si ile aynı.

            media_service_url: env::var("MEDIA_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: MEDIA_SERVICE_TARGET_GRPC_URL")?,
            proxy_service_url: env::var("PROXY_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            registrar_service_url: env::var("REGISTRAR_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: REGISTRAR_SERVICE_TARGET_GRPC_URL")?,
            user_service_url: env::var("USER_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            dialplan_service_url: env::var("DIALPLAN_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: DIALPLAN_SERVICE_TARGET_GRPC_URL")?,
            
            rabbitmq_url: env::var("RABBITMQ_URL").context("ZORUNLU: RABBITMQ_URL")?,
            redis_url: env::var("REDIS_URL").context("ZORUNLU: REDIS_URL")?,
            
            public_ip: sbc_public_ip, // Kendi Public IP'sini de buradan alıyor.
            sip_realm: env::var("SIP_SIGNALING_SERVICE_REALM").unwrap_or_else(|_| "sentiric_demo".to_string()),

            env: env::var("ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            service_version: env::var("SERVICE_VERSION").unwrap_or_else(|_| "1.4.3".to_string()), // Versiyonu güncelleyelim
            
            cert_path: env::var("B2BUA_SERVICE_CERT_PATH").context("ZORUNLU: B2BUA_SERVICE_CERT_PATH")?,
            key_path: env::var("B2BUA_SERVICE_KEY_PATH").context("ZORUNLU: B2BUA_SERVICE_KEY_PATH")?,
            ca_path: env::var("GRPC_TLS_CA_PATH").context("ZORUNLU: GRPC_TLS_CA_PATH")?,
        })
    }
}