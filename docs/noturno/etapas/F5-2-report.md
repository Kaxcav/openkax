# Etapa F5-2 — `openkax report`

Entregáveis (arquivo `src/report.rs`, despacho em `src/main.rs`):
1. `openkax report [--day|--week|--since AAAA-MM-DD] [--json]` lendo o ledger JSONL do diretório de dados do `Platform` (não mude o formato do ledger).
2. Por classe e por provider/modelo: requests, taxa de ok, outcomes, latência e TTFT p50/p95, tokens in/out, quantas caíram em tier credit/paid, fallbacks. Para `kind:"task"`: aceitação em 1ª passada por classe/provider (base das métricas do PLANO §2.3).
3. Linhas malformadas são contadas e ignoradas (não derrubam o relatório).
4. Testes com ledger de fixture (incluindo linha malformada e período vazio).
5. Se a F2-6 já tiver um `report` mínimo no MCP, faça a tool `report` usar este módulo.
