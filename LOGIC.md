# 🔄 Sentiric B2BUA Service - Mantık Mimarisi (Final)

**Rol:** Hat Operatörü. Medya sonlandırma ve Olay tetikleme noktası.

## 1. Çağrı Karşılama Akışı (Inbound Handler)

1.  **INVITE Gelir:**
    *   `100 Trying` gönder.
    *   `media-service`'ten port kirala (`AllocatePort`).

2.  **Medya Hazırlığı (Hole Punching):**
    *   Arayanın SDP'sindeki IP'yi al.
    *   `media-service`'e "Bu IP'ye boş paket at" (NAT Delme) emrini ver.

3.  **Cevaplama:**
    *   `200 OK` gönder (SDP içinde Public IP ile).
    *   **KRİTİK ADIM:** `RabbitMQ`'ya `call.started` olayını bas. (İçinde CallID, Arayan, Aranan bilgisi ile).

4.  **Yaşam Döngüsü:**
    *   Çağrı sürdüğü sürece (SIP Session) hattı açık tut.
    *   `BYE` gelirse `media-service`'teki portu serbest bırak ve `call.ended` olayını bas.

## 2. Olay Şeması (RabbitMQ Payload)

B2BUA'nın attığı topu `agent-service` karşılar.

```json
{
  "eventType": "call.started",
  "callId": "...",
  "mediaInfo": {
    "serverRtpPort": 10050,
    "callerRtpAddr": "1.2.3.4:5678"
  },
  "dialplanResolution": { ... }
}
```

---
