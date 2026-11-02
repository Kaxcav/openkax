# Etapa F5-1 — `openkax doctor` em Rust

Objetivo: substituir gradualmente `doctor.sh`/`doctor.ps1`.

Entregáveis (arquivo `src/doctor.rs`, despacho em `src/main.rs`):
1. `openkax doctor [--chat] [--provider <id>] [--json]`. Usa o parser de config existente; do ambiente lê só a presença das variáveis (nunca imprime valor).
2. Para cada provider habilitado: catálogo (`/models`; Cloudflare em `/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/models/search`). 2xx que não é JSON → FAIL. 401/403 → FAIL (auth). Conta modelos; WARN quando um `upstream` usado nas rotas não está no catálogo.
3. `--chat`: 1 chat mínimo (max_tokens 8) por modelo habilitado, aplicando quirks pelo cliente de provider existente; mostra status, TTFT e os headers de limite pelo mapeamento existente (`x-ratelimit-*` e `RateLimit`/`RateLimit-Policy`).
4. Exit code ≠ 0 se algum provider habilitado der FAIL. `--json` para máquina.
5. Testes com wiremock (base_url apontando para o mock via config de teste): JSON ok; 2xx text/plain → FAIL; 401 → FAIL; upstream ausente → WARN; headers parseados. **Não rode contra provider real.**
