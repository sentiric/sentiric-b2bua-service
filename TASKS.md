# 🔄 Sentiric B2BUA Service - Görev Listesi

Bu servisin mevcut ve gelecekteki tüm geliştirme görevleri, platformun merkezi görev yönetimi reposu olan **`sentiric-tasks`**'ta yönetilmektedir.

➡️ **[Aktif Görev Panosuna Git](https://github.com/sentiric/sentiric-tasks/blob/main/TASKS.md)**

---
Bu belge, servise özel, çok küçük ve acil görevler için geçici bir not defteri olarak kullanılabilir.

## Faz 1: Minimal İşlevsellik (INFRA-02)
- [x] Temel Rust projesi ve Dockerfile oluşturuldu.
- [x] gRPC sunucusu iskeleti (`UnimplementedB2BUAService`) hazırlandı.
- [ ] Agent, Proxy, Registrar ve Media servislerine gRPC istemcileri eklenecek. (INFRA-03)
- [ ] Çağrı durumu yönetimi (Aktif Çağrı tablosu) eklenecek. (ORCH-01)