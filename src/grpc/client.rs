// sentiric-b2bua-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::{Context, Result};
use sentiric_contracts::sentiric::media::v1::media_service_client::MediaServiceClient;
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::user::v1::user_service_client::UserServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;
use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity, Endpoint};
use tracing::{info, warn};

#[derive(Clone)]
pub struct InternalClients {
    pub media: MediaServiceClient<Channel>,
    pub registrar: RegistrarServiceClient<Channel>,
    pub user: UserServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>,
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!("🔌 İç servislere bağlanılıyor (Lazy Connect + mTLS)...");

        // TLS Yapılandırmasını Hazırla
        let tls_config = if !config.ca_path.is_empty() {
            match load_tls_config(config).await {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    warn!("⚠️ mTLS sertifikaları yüklenemedi, güvensiz mod denenecek: {}", e);
                    None
                }
            }
        } else {
            warn!("⚠️ mTLS CA yolu boş, güvensiz bağlantılar kullanılacak.");
            None
        };
        
        // Endpoint'leri oluştur (Henüz bağlanmaz, ilk istekte bağlanır)
        let media_channel = connect_endpoint(&config.media_service_url, &tls_config).await?;
        let registrar_channel = connect_endpoint(&config.registrar_service_url, &tls_config).await?;
        let user_channel = connect_endpoint(&config.user_service_url, &tls_config).await?;
        let dialplan_channel = connect_endpoint(&config.dialplan_service_url, &tls_config).await?;

        info!("✅ Tüm gRPC istemci Endpoint'leri başarıyla yapılandırıldı.");

        Ok(Self {
            media: MediaServiceClient::new(media_channel),
            registrar: RegistrarServiceClient::new(registrar_channel),
            user: UserServiceClient::new(user_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel),
        })
    }
}

async fn connect_endpoint(url: &str, tls_config: &Option<ClientTlsConfig>) -> Result<Channel> {
    // URL'i parse et
    let uri = url.parse::<tonic::transport::Uri>()
        .with_context(|| format!("Geçersiz URL: {}", url))?;
    
    let mut endpoint = Endpoint::from(uri);

    // Eğer HTTPS ise TLS ayarlarını ekle
    if url.starts_with("https") {
        if let Some(tls) = tls_config {
            endpoint = endpoint.tls_config(tls.clone())?;
        } else {
            warn!("HTTPS URL için TLS konfigürasyonu bulunamadı: {}", url);
        }
    }

    // [KRİTİK]: connect_lazy() kullanarak servisin ayakta olmasını beklemiyoruz.
    // İlk istekte otomatik bağlanacak.
    Ok(endpoint.connect_lazy())
}

async fn load_tls_config(config: &AppConfig) -> Result<ClientTlsConfig> {
    let cert = tokio::fs::read(&config.cert_path).await.context("İstemci sertifikası okunamadı")?;
    let key = tokio::fs::read(&config.key_path).await.context("İstemci anahtarı okunamadı")?;
    let identity = Identity::from_pem(cert, key);
    
    let ca_cert = tokio::fs::read(&config.ca_path).await.context("CA sertifikası okunamadı")?;
    let ca_certificate = Certificate::from_pem(ca_cert);

    Ok(ClientTlsConfig::new()
        .domain_name("sentiric.cloud") // Genel SNI
        .ca_certificate(ca_certificate)
        .identity(identity))
}