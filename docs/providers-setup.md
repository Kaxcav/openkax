# Criação das chaves dos providers

> Atualizado 2026-11-02 (alinhado ao ADR 0004 e ao smoke F0-21). UIs mudam; se um botão não estiver onde diz, procure pelo nome entre aspas.
>
> Depois de cada chave: edite o `.env` → cola → `./doctor.sh` (Linux/WSL) ou `.\doctor.ps1` (Windows).

## Regras antes de começar

- **Nunca cole chave em chat, em prompt do Claude Code ou em commit.** O `.env` é gitignored; confira com `git check-ignore .env`.
- **Salve o `.env` com quebra de linha LF.** Valor colado pelo lado Windows com CRLF fica com `\r` no fim e a autenticação falha mesmo com a chave certa. No VS Code/Antigravity: barra inferior → "CRLF" → "LF". No WSL: `sed -i 's/\r$//' .env`. (O gateway tolera CRLF via dotenvy; o `doctor.sh` em bash, não.)
- Diagnóstico seguro (só nomes e tamanhos, nunca valores):
  ```bash
  awk '/^[A-Z0-9_]+=/{v=substr($0,index($0,"=")+1); printf "%s len=%d crlf=%s\n", substr($0,1,index($0,"=")-1), length(v), (v ~ /\r$/ ? "SIM" : "não")}' .env
  ```
- Provider "free" que exige cartão de crédito fica **fora** (decisão de 2026-11-02, ADR 0004). Ver a última seção.

## Base (já configurados)

Gemini (AI Studio), Mistral (La Plateforme), Groq, OpenRouter (US$10 lifetime → 1000 RPD nos `:free`), DeepSeek (pago, centavos — sempre último degrau).

---

## 1. Codestral

O gateway usa o **codestral pela La Plateforme**, com a mesma `MISTRAL_API_KEY` (`https://api.mistral.ai/v1`, modelo `codestral-latest`). Medido no smoke: **125 RPM / 625k TPM por modelo**, TTFT ~0,5 s, tool calling OK — é a espinha dorsal do `kax/coder-smart`. **Não precisa de chave extra.**

O endpoint dedicado `https://codestral.mistral.ai/v1` (chave `CODESTRAL_API_KEY`, quota separada, suporta FIM) **não é usado hoje**. Fica registrado como possível degrau extra de capacidade no futuro.

## 2. NVIDIA API Catalog / NIM (5 min)
Endpoint `https://integrate.api.nvidia.com/v1`, ~40 RPM, limite varia por modelo e tráfego. ToS: dev/test, **não mandar dado confidencial**.

1. `build.nvidia.com` → **"Login"** → cria conta NVIDIA Developer (e-mail, sem cartão). Confirma e-mail.
2. Avatar (canto superior direito) → **"API Keys"** → **"Generate API Key"**.
3. Copia (começa com `nvapi-`) → `.env`: `NVIDIA_API_KEY=`
4. IDs de modelo são `org/modelo` como aparecem no catálogo. Nem todo modelo listado em `/models` responde (ex.: `kimi-k2.6` deu 404) — o gateway desliga só o modelo.

Lento em horário de pico (fila compartilhada): o `providers.toml` usa `first_token_timeout_ms = 20000` nesses modelos. Não existe aumento de limite no free.

## 3. Z.ai (Zhipu) — GLM Flash (3 min)
Endpoint `https://api.z.ai/api/paas/v4` (plataforma internacional; **não** é `bigmodel.cn`).

1. `z.ai/model-api` → **"Get API Key"** / **"Sign up"** (e-mail ou Google).
2. Console → **"API Keys"** → **"Create new API key"** → `.env`: `ZAI_API_KEY=`
3. Ignore o "Coding Plan" (assinatura paga, outro produto).
4. Modelos free: `glm-4.7-flash`, `glm-4.5-flash`. Concorrência 1. Thinking desligado via `thinking={type:"disabled"}` (quirk no TOML).

## 4. Cloudflare Workers AI (5 min)
Endpoint `https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1`. 10k neurons/dia (neuron ≠ token; o gateway faz pacing por `rpd`).

1. `dash.cloudflare.com` → **Account ID**: aparece na URL (`dash.cloudflare.com/<32 hex>/...`) ou em **"Workers & Pages"** → barra lateral. → `.env`: `CLOUDFLARE_ACCOUNT_ID=`
2. Avatar → **"My Profile"** → **"API Tokens"** → **"Create Token"** → **"Create Custom Token"**.
3. Permissions: **Account → Workers AI → Read**. Account Resources: Include → sua conta.
4. **"Continue to summary"** → **"Create Token"** → copia (só aparece uma vez) → `.env`: `CLOUDFLARE_API_TOKEN=`
5. **Atenção aos nomes:** são duas variáveis distintas. Token salvo com o nome do account id foi exatamente o erro de 2026-11-02.
6. IDs `@cf/...` vêm do catálogo vivo em `/ai/models/search` (não em `/v1/models`), ignorando modelos com deprecation — o `doctor` já faz isso. Modelo descontinuado responde **410** e o gateway desliga só o modelo.

## 5. Cohere — trial (3 min)
Endpoint OpenAI-compat `https://api.cohere.ai/compatibility/v1`. 1.000 chamadas/mês, 20 RPM, **não comercial**.

1. `dashboard.cohere.com` → **"Sign up"**.
2. Menu lateral → **"API Keys"** → copia a Trial key (ou **"+ New Trial key"**) → `.env`: `COHERE_API_KEY=`
3. Modelo usado: `command-a-03-2025` no `kax/reviewer`. **Não** use `command-a-plus` (vaza raciocínio no content). O TOML liga a quirk `ascii_json` (prompt em português com UTF-8 cru gerava saída degenerada).

## 6. SambaNova Cloud (3 min)
Endpoint `https://api.sambanova.ai/v1`. Por modelo: 20 RPM, **20 RPD**, 200k TPD.

1. `cloud.sambanova.ai` → **"Sign up"**.
2. Menu lateral → **"API Keys"** → **"Generate New API Key"** → `.env`: `SAMBANOVA_API_KEY=`
3. **Não** adicione método de pagamento (move para o Developer tier, que cobra).
4. Em horário de pico responde 429 "high demand" (capacidade deles, não quota sua) — o gateway trata como cooldown do modelo.

---

## Depois de tudo
```bash
cd ~/openkax && ./doctor.sh        # Windows: .\doctor.ps1
```
Cada provider deve sair `[OK]`. `403` com "check your network settings" ou resposta 2xx que não é JSON = bloqueio de rede/VPN (o ProtonVPN do notebook bloqueia o Groq), não a chave.

## Fora — exigem cartão ou não existem mais (ADR 0004)

- **GitHub Models**: aposentado em 30/07/2026 (playground, catálogo, inference API e BYOK desligados para todos). Removido do gateway. Revogue o PAT que era usado nele.
- **Vercel AI Gateway**: responde 403 "AI Gateway requires a valid credit card on file" mesmo para modelos de preço 0. Fica no `providers.toml` com `enabled = false`, fora das rotas. Não cadastrar cartão.
- **Alibaba Model Studio (Qwen/DashScope)**: pode exigir cartão na verificação e a quota free expira em 90 dias. Não adotado.
- **Cerebras**: virou trial com cartão. Fora.

## Rotação
Toda chave que passou por chat (inclusive colada em texto puro) deve ser rotacionada ao fechar a Fase 0.
