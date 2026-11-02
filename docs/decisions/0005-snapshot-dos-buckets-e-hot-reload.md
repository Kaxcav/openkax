# 0005 — Snapshot dos buckets e hot-reload do providers.toml

Data: 2026-11-02
Status: aceita

## Contexto

Os buckets moravam só na memória: um restart devolvia RPD/TPD e cotas mensais cheios, e o gateway voltava a gastar cota que o upstream já tinha contado (Cohere: 1000 chamadas/mês por chave). Mudar o `providers.toml` exigia reiniciar, com o mesmo efeito. O plano pede ledger JSONL e "buckets em snapshot JSON", sem SQLite.

## Decisão

- **Snapshot** (`Scheduler::export_state`/`restore_state`, tipo `BucketsSnapshot`, versão 1):
  - Uma entrada por escopo, chave `"provider"` ou `"provider/modelo"`: nível de RPM/TPM, janela IETF aprendida (`cap`, `per_sec`, `level`), contadores RPD/TPD e mensais (`used` + índice da janela), cooldown restante e `chars_per_token` do modelo.
  - Tempos em segundos UTC de parede (`saved_at_utc`), porque `Instant` não sobrevive a restart. No restore, RPM/TPM/janela reabastecem pelo tempo decorrido; contador de dia/mês já virado zera; cooldown vencido some.
  - O restore só mexe nos buckets que a config ATUAL declara: escopo desconhecido é ignorado, bucket novo começa cheio, nível acima do teto novo é cortado.
  - `Disabled` nunca entra no snapshot: por definição vale "até reload".
  - O scheduler continua sem I/O. Arquivo: `AppState::save_state`/`restore_state`, em `<state_dir>/buckets.json` (`%LOCALAPPDATA%\OpenKax\state` no Windows, `$XDG_DATA_HOME/openkax/state` no Linux), escrita atômica (tmp + rename). Salvo a cada 60 s e no shutdown; restaurado no start. JSON inválido é logado e ignorado.
- **Mensais** (N6): `requests_per_month`/`tokens_per_month` viram buckets de verdade, com reset às 00:00 UTC do dia 1. `tokens_per_month` abaixo da reserva = `NeverFits`; esgotado = `NotNow` até a virada do mês. `/quota` expõe `requests_month_remaining`/`tokens_month_remaining` nos dois escopos.
- **Hot-reload** (`server::watch_config`, crate `notify`):
  - Observa o diretório do arquivo (editores trocam o arquivo por rename), com debounce de 300 ms.
  - TOML inválido → warning no log, config viva mantida.
  - O estado do router é `RwLock<Arc<Live>>`, onde `Live` = config + `${VAR}` resolvidas + segredos + scheduler + cliente HTTP. `reload` monta a geração nova, aplica `export_state()` da antiga e troca o ponteiro.
  - Cada request pega uma `View` (uma geração) no começo e a usa até o fim; reservations em voo fecham no scheduler antigo. Esse débito tardio não passa para a geração nova (perda de no máximo as requests em voo no instante do reload).

## Consequências

- Restart e reload não devolvem cota já gasta. A cota mensal da Cohere passa a ser respeitada de fato, não só espalhada por `rpd`.
- `request_timeout_ms` passa a valer no reload (o cliente HTTP é recriado com a geração).
- Os segredos vêm do ambiente do processo: `.env` alterado só vale depois de restart.
- Snapshot de versão diferente é ignorado inteiro (sem migração na v1).
