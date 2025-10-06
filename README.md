# 🔄 Sentiric B2BUA Service

[![Status](https://img.shields.io/badge/status-vision-lightgrey.svg)]()
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)]()
[![Protocol](https://img.shields.io/badge/protocol-gRPC_&_SIP-green.svg)]()

**Sentiric B2BUA (Back-to-Back User Agent) Service**, Sentiric platformunda SIP çağrı başlatma ve aktarma (transfer) mantığını yöneten merkezi bir B2BUA çekirdeğidir. AI Agent'tan gelen talepler üzerine, yeni bir arama başlatır (Outbound Dialing) veya mevcut bir çağrıyı başka bir hedefe aktarır (Transfer, REFER).

Bu servis, SIP sinyalleşme işlemini tamamladıktan sonra medya akışını başlatması için `media-service`'i koordine eder.

## 🎯 Temel Sorumluluklar

1.  **Çağrı Başlatma (InitiateCall):** AI Agent'tan gelen istek üzerine harici bir SIP hedefiyle yeni bir çağrı oturumu başlatır ve bu çağrıya ilişkin sinyalleşmeyi yönetir.
2.  **Çağrı Transferi (TransferCall):** Mevcut bir aktif çağrıyı (örn: kullanıcının AI ile konuştuğu çağrı) üçüncü bir tarafa (başka bir Agent, harici numara) aktarır.
3.  **Medya Koordinasyonu:** Çağrı kurulduğunda, medya akışının doğru şekilde kurulması için `media-service`'ten RTP portu tahsisini ister.
4.  **SIP Uç Nokta Çözümlemesi:** Çağrı hedeflerini bulmak için `registrar-service`'ten destek alır.

## 🛠️ Teknoloji Yığını

*   **Dil:** Rust (Yüksek performanslı telkom protokol işleme için)
*   **Servisler Arası İletişim:** gRPC (Tonic)
*   **SIP Yönlendirme:** `sentiric-proxy-service` (giden çağrı paketlerini yönlendirmek için)
*   **Durum/Olay Yönetimi:** Redis ve RabbitMQ (çağrı durumunu ve olaylarını yayınlamak için)

## 🔌 API Etkileşimleri

*   **Gelen (Sunucu):**
    *   `sentiric-agent-service` (gRPC): `InitiateCall`, `TransferCall` RPC'leri.
*   **Giden (İstemci):**
    *   `sentiric-proxy-service` (gRPC): Giden SIP paketlerini göndermek için.
    *   `sentiric-media-service` (gRPC): RTP port tahsisi ve serbest bırakılması.
    *   `sentiric-registrar-service` (gRPC): Hedef uç noktayı aramak için.

---
## 🏛️ Anayasal Konum

Bu servis, [Sentiric Anayasası'nın](https://github.com/sentiric/sentiric-governance) **Core Logic Layer**'ında yer alan yeni SIP Protokol Yönetimi bileşenidir.