# kax-workers no WSL (verify com sandbox)

O verify do `kax-workers` só roda isolado no Linux, com `bwrap` (ADR 0008).
No Windows nativo o padrão `sandbox = "bwrap"` recusa e a tarefa vai para
`needs_lead`. Para delegar a partir do Claude Code no Windows, rode o
`openkax mcp` dentro do WSL.

## 1. Preparar a distro

```bash
sudo apt-get update && sudo apt-get install -y bubblewrap build-essential git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # se ainda não tiver rust
bwrap --unshare-all --ro-bind / / --dev /dev true && echo "bwrap ok"
```

Se o `bwrap` reclamar de user namespace (Ubuntu 24.04 com AppArmor), libere
com `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (e grave
em `/etc/sysctl.d/` para valer no boot) ou instale um perfil AppArmor para o
`bwrap`.

## 2. Repo e build Linux

Clone o repo no ext4 (`~/openkax`), não em `/mnt/c` ou `/mnt/d`: é mais rápido
e funciona mesmo com o automount desligado (seção 5).

```bash
git clone https://github.com/Kaxcav/openkax ~/openkax
cd ~/openkax
cp .env.example .env   # só as chaves dos providers das classes kax/* (ADR 0007)
cargo build --release
./target/release/openkax doctor   # a linha "[OK] sandbox" confirma o bwrap
```

O repo que você vai delegar também tem de estar acessível de dentro do WSL,
de preferência no ext4. Nas chamadas `delegate`/`delegate_batch`, o campo
`repo` é um **caminho Linux** (`/home/<user>/projeto`), nunca `D:\...`.

## 3. Registrar no Claude Code (Windows)

Forma mínima:

```powershell
claude mcp add kax-workers -- wsl -d <distro> -- ~/openkax/target/release/openkax mcp
```

`<distro>` é o nome exibido por `wsl -l -v` (ex.: `Ubuntu-24.04`). Passe o
`-d` sempre: a distro padrão pode ser outra (o Docker Desktop instala a
`docker-desktop`, que nem tem bash).

O `openkax mcp` lê o `.env` do diretório atual e o `providers.toml` de
`OPENKAX_CONFIG` (padrão `./providers.toml`), e o verify precisa do `cargo` no
`PATH`. Por isso a forma recomendada fixa os três:

```powershell
claude mcp add kax-workers -- wsl -d <distro> -- bash -c '. ~/.cargo/env && cd ~/openkax && OPENKAX_CONFIG=$HOME/openkax/providers.toml exec ./target/release/openkax mcp'
```

Use `bash -c`, não `bash -lc`: o stdout é exclusivo do protocolo MCP, e um
`.profile` que imprima algo quebra o handshake.

## 4. `.kax/verify.toml` do repo delegado

Sem o arquivo, vale `sandbox = "bwrap"`. Para baixar dependências antes do
verify offline (a caixa não tem rede):

```toml
[[language]]
name = "rust"
globs = ["**/*.rs", "Cargo.toml"]
commands = ["cargo test"]
prefetch = ["cargo fetch --locked"]
```

O prefetch roda com rede, fora da caixa, sobre o `base_ref` e antes do diff
do operário. Verify que use `git` não funciona na caixa (ADR 0008).

## 5. Opcional: distro dedicada

Para que nem um processo fora da caixa (o prefetch, ou o próprio `openkax`)
enxergue os discos e executáveis do Windows, use uma distro só para o
`kax-workers`:

```powershell
# WSL recente:
wsl --install Ubuntu-24.04 --name kax-workers
# ou, em qualquer versão, a partir de uma distro limpa:
wsl --export Ubuntu-24.04 $env:TEMP\base.tar
wsl --import kax-workers $env:LOCALAPPDATA\kax-workers $env:TEMP\base.tar
```

Dentro dela, `/etc/wsl.conf`:

```ini
[automount]
enabled = false   # sem /mnt/c, /mnt/d: o repo fica no ext4

[interop]
enabled = false   # processos Linux não executam .exe do Windows
appendWindowsPath = false
```

Depois `wsl --terminate kax-workers` para aplicar. O registro no Claude Code
continua funcionando (o `wsl.exe` chamado do Windows não depende do interop
dentro da distro). Repita as seções 1–3 nessa distro, usando `-d kax-workers`.
