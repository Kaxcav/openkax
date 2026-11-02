# 0001 — Ambientes de desenvolvimento e alvo de release

Data: 2026-11-02
Status: aceita

## Contexto

O desenvolvimento acontece em duas máquinas:

- **PC de casa** (Windows, sem restrição): Rust MSVC nativo, pode rodar o `.exe` final.
- **Notebook alugado** (Windows com **WDAC/Device Guard em modo enforce**, política corporativa não removível): só binários assinados pela Microsoft rodam no host. Git for Windows, Node para Windows, o desktop app do Claude e **qualquer `.exe` que o OpenKax gere** são bloqueados. Todo o trabalho acontece dentro do **WSL2 (Ubuntu)**, repo em `~/openkax` no ext4 do WSL (nunca em `/mnt/c/`).

O produto final continua sendo um binário Windows leve (< 8 MB, RSS < 10 MB), mas esse alvo não pode ser executado nem testado no notebook.

## Decisão

1. **O código é cross-platform desde o primeiro commit.** Nada de `#[cfg(windows)]` fora do módulo de integração com o SO (tray, autostart, paths de `%LOCALAPPDATA%`), que fica isolado atrás de um trait `Platform` com implementação Linux equivalente (`$XDG_DATA_HOME`/`~/.local/share/openkax`).
2. **Dev e testes rodam em Linux** (WSL no notebook, e também no PC se quiser). `cargo test`, `clippy` e os critérios de aceite das Fases 0–4 são validados em Linux primeiro.
3. **No notebook, o gateway roda dentro do WSL** (`127.0.0.1:8000`). Clientes no Windows (VS Code com Continue/Cline, navegador) alcançam via `localhost:8000` pelo encaminhamento de porta padrão do WSL2. Claude Code roda **dentro do WSL** (instalador nativo Linux), nunca no host.
4. **O `.exe` de release é construído por GitHub Actions** (`windows-latest`, MSVC) e opcionalmente no PC de casa. O notebook nunca constrói nem executa o `.exe`. Os critérios "binário < 8 MB / RSS < 10 MB" são medidos no artefato do CI e no PC de casa.
5. Cross-compile de WSL para `x86_64-pc-windows-gnu` fica como opção de conveniência, não como caminho oficial de release.

## Adendo (2026-11-02) — PC de casa como segundo ambiente de dev

O PC de casa (`kaxcavmaster`) passa a ser ambiente de dev **nativo Windows**: sem WDAC, sem VPN, repo em `D:\dev\openkax`, PowerShell e Git Bash permitidos, toolchain MSVC. Ali:

- `cargo test` e `cargo clippy --all-targets -- -D warnings` rodam no Windows e precisam passar como no Linux. Testes que dependem de caminho absoluto usam uma base portátil (`current_dir()`), não `/x/...`.
- O `.exe` roda localmente (`127.0.0.1:8000`): é onde se medem tamanho, RSS ocioso e o smoke por classe antes do CI.
- `WindowsPlatform` usa `%LOCALAPPDATA%\OpenKax` (cai em `%USERPROFILE%\AppData\Local\OpenKax` se a variável faltar ou for relativa) para config, `ledger\`, `state\` e `logs\`.
- Rede doméstica sem proxy: é daqui que se conferem os catálogos que a rede do notebook bloqueia (Groq, GitHub Models).

O **CI continua sendo a fonte do `.exe` de release**; o build local serve para medir e testar. O notebook segue só com WSL/bash.

## Consequências

- Ganha-se um binário Linux de graça (útil para servidores/Railway no futuro).
- Toda feature de Fase 5 que toque o SO (tray, autostart) precisa de implementação Linux ou de um no-op explícito.
- O workflow de CI entra ainda na Fase 0 (ao fechar o core) para que o artefato Windows exista desde cedo.
- Em máquina com WDAC o produto é inutilizável como `.exe` não assinado; assinatura de código fica registrada como possibilidade futura, fora de escopo agora.
