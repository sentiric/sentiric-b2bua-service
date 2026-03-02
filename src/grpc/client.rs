// sentiric-b2bua-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::{Context, Result};
use sentiric_contracts::sentiric::media::v1::media_service_client::MediaServiceClient;
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::user::v1::user_service_client::UserServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;
use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity};
use std::time::Duration;
use tracing::{info, error, warn};

#[derive(Clone)]
pub struct InternalClients {
    pub media: MediaServiceClient<Channel>,
    pub registrar: RegistrarServiceClient<Channel>,
    pub user: UserServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>,
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!("🔌 İç servislere bağlanılıyor (mTLS + Agresif KeepAlive)...");

        let media_channel = match create_secure_channel(&config.media_service_url, "media-service", config).await {
            Ok(c) => c,
            Err(e) => {
                error!("❌ Media Service bağlantı hatası: {:#}", e);
                return Err(e);
            }
        };

        let registrar_channel = create_secure_channel(&config.registrar_service_url, "registrar-service", config).await?;
        let user_channel = create_secure_channel(&config.user_service_url, "user-service", config).await?;
        let dialplan_channel = create_secure_channel(&config.dialplan_service_url, "dialplan-service", config).await?;

        info!("✅ Tüm gRPC istemcileri başarıyla oluşturuldu.");

        Ok(Self {
            media: MediaServiceClient::new(media_channel),
            registrar: RegistrarServiceClient::new(registrar_channel),
            user: UserServiceClient::new(user_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel),
        })
    }
}

async fn create_secure_channel(url: &str, server_name: &str, config: &AppConfig) -> Result<Channel> {
    let target_url = if url.starts_with("http") {
        if url.starts_with("http://") {
             warn!("⚠️ Güvensiz URL tespit edildi ({}), HTTPS'e zorlanıyor.", url);
             url.replace("http://", "https://")
        } else {
            url.to_string()
        }
    } else {
        format!("https://{}", url)
    };

    let cert = tokio::fs::read(&config.cert_path).await
        .with_context(|| format!("İstemci Sertifikası okunamadı: {}", config.cert_path))?;
    
    let key = tokio::fs::read(&config.key_path).await
        .with_context(|| format!("İstemci Anahtarı okunamadı: {}", config.key_path))?;
    
    let identity = Identity::from_pem(cert, key);

    let ca_cert = tokio::fs::read(&config.ca_path).await
        .with_context(|| format!("CA Sertifikası okunamadı: {}", config.ca_path))?;
    
    let ca_certificate = Certificate::from_pem(ca_cert);

    let tls_config = ClientTlsConfig::new()
        .domain_name(server_name)
        .ca_certificate(ca_certificate)
        .identity(identity);

    info!("🔗 Bağlanılıyor: {} (SNI: {})", target_url, server_name);

    // [MÜKEMMEL TELEKOM OPTİMİZASYONU]: gRPC HTTP/2 Keep-Alive ve Timeout Tuning
    // Amaç: Soğuk başlangıç (Cold start) ve NAT/Firewall sessiz düşmelerini (Silent Drop) engellemek.
    let channel = Channel::from_shared(target_url)?
        .connect_timeout(Duration::from_secs(2)) // Bağlantı en fazla 2 saniye sürsün
        .keep_alive_while_idle(true) // Trafik yokken bile HTTP/2 PING at!
        .http2_keep_alive_interval(Duration::from_secs(10)) // 10 saniyede bir PING at (Firewall'u delik tut)
        .keep_alive_timeout(Duration::from_secs(3)) // 3 saniyede PING cevabı gelmezse kopar ve yeniden bağlan
        .tcp_nodelay(true) // Nagle algoritmasını kapat (Düşük gecikme)
        .tcp_keepalive(Some(Duration::from_secs(10))) // OS seviyesinde de TCP Keep-Alive
        .tls_config(tls_config)?
        .connect()
        .await
        .with_context(|| format!("{} servisine gRPC bağlantısı kurulamadı", server_name))?;

    Ok(channel)
}