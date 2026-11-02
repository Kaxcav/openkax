# Criação das chaves dos providers

> Atualizado 2026-11-02. UIs mudam; se um botão não estiver onde diz, procure pelo nome entre aspas. Depois de cada chave: `nano .env` → cola → `./doctor.sh`. **Nunca cole chave em chat ou em prompt do Claude Code.**

## Já feitos
Gemini, Mistral (La Plateforme), Groq, GitHub Models, OpenRouter (US$10), DeepSeek (US$5).

---

## 1. Codestral — endpoint dedicado (2 min)
Mesma conta Mistral, **chave e quota separadas** da La Plateforme. Endpoint `https://codestral.mistral.ai/v1`.

1. `console.mistral.ai` → já logado.
2. Menu lateral → **"Codestral"** (se não aparecer: "API Keys" → aba/seção "Codestral").
3. **"Request access"** / **"Generate key"** — pode pedir verificação de telefone (você já fez pra La Plateforme).
4. Copia a chave → `.env`: `CODESTRAL_API_KEY=`
5. A chave da La Plateforme **não funciona** nesse endpoint, e vice-versa.

Limites ~30–60 RPM. Modelo: `codestral-latest`. Suporta FIM (`/v1/fim/completions`).

## 2. NVIDIA API Catalog / NIM (5 min)
Endpoint `https://integrate.api.nvidia.com/v1`, ~40 RPM, sem cap diário publicado. ToS: dev/test, **não mandar dado confidencial**.

1. `build.nvidia.com` → **"Login"** → cria conta NVIDIA Developer (e-mail, sem cartão). Confirma e-mail.
2. Logado, clica no avatar (canto superior direito) → **"API Keys"** → **"Generate API Key"**.
   Alternativa: abre qualquer modelo (ex.: `deepseek-ai/deepseek-v4-flash`) → **"Get API Key"** / "Build with this NIM" → gera.
3. Copia (começa com `nvapi-`) → `.env`: `NVIDIA_API_KEY=`
4. IDs de modelo são `org/modelo` como aparecem no catálogo (ex.: `qwen/qwen3-coder-480b-a35b-instruct`, `nvidia/llama-3.3-nemotron-super-49b-v1.5`). O `doctor` lista os visíveis.

Lento em horário de pico (infra compartilhada). Não pedir aumento de limite no fórum — não existe pra free.

## 3. Vercel AI Gateway (5 min + cartão)
Endpoint `https://ai-gateway.vercel.sh/v1`. US$5 de crédito a cada 30 dias enquanto você **nunca comprar crédito**.

1. `vercel.com` → **"Sign Up"** com GitHub (plano Hobby serve).
2. Dashboard → aba/menu **"AI Gateway"** → **"API Keys"** → **"Create Key"** → copia → `.env`: `VERCEL_AI_GATEWAY_KEY=`
3. Pra liberar os US$5: a tela vai pedir **cartão internacional válido** só pra verificar identidade (Settings → Billing → Payment Method). **Não** compra crédito, **não** ativa auto top-up — qualquer compra encerra o free permanente.
4. O ciclo de 30 dias começa na primeira request.
5. IDs no formato `provider/modelo` (ex.: `deepseek/deepseek-v3.2`, `zai/glm-4.7`, `moonshotai/kimi-k2`). O free tier cobre **um subconjunto** do catálogo — modelo fora dele devolve `403 RestrictedModelsError`; o scheduler trata como provider pulado pra aquele modelo.

Alternativa por CLI (no WSL): `npx vercel login && npx vercel ai-gateway api-keys create --name openkax`.

## 4. Z.ai (Zhipu) — GLM-4.7-Flash (3 min)
Endpoint `https://api.z.ai/api/paas/v4` (plataforma internacional; **não** é `bigmodel.cn`).

1. `z.ai/model-api` → **"Get API Key"** / **"Sign up"** (e-mail ou Google).
2. Console → **"API Keys"** → **"Create new API key"** → copia → `.env`: `ZAI_API_KEY=`
3. Ignora "Coding Plan" (assinatura paga, outra coisa).
4. Modelo free: `glm-4.7-flash` (e `glm-4.5-flash` enquanto existir). 200K ctx, concorrência 1, **contexto > 8K é estrangulado** — só pacotes pequenos.

## 5. Cloudflare Workers AI (5 min, conta que você já tem)
Endpoint `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1`. 10k Neurons/dia.

1. `dash.cloudflare.com` → **Account ID**: aparece na barra lateral direita de qualquer domínio, ou em **"Workers & Pages"** → Overview → "Account details". Copia → `.env`: `CLOUDFLARE_ACCOUNT_ID=`
2. Avatar → **"My Profile"** → **"API Tokens"** (`dash.cloudflare.com/profile/api-tokens`) → **"Create Token"** → **"Create Custom Token"**.
3. Permissions: **Account → Workers AI → Read**. Account Resources: Include → sua conta. (Se inferência der 401/403, recria com **Edit**.)
4. **"Continue to summary"** → **"Create Token"** → copia (só aparece uma vez) → `.env`: `CLOUDFLARE_API_TOKEN=`
5. IDs `@cf/...` (ex.: `@cf/qwen/qwen2.5-coder-32b-instruct`, `@cf/zai-org/glm-4.7-flash`, `@cf/openai/gpt-oss-120b`). Listagem é em `/ai/models/search`, não em `/v1/models` — o `doctor` já usa a certa.

## 6. Cohere — trial (3 min)
Endpoint OpenAI-compat `https://api.cohere.ai/compatibility/v1`. 1.000 chamadas/mês, 20 RPM, não comercial.

1. `dashboard.cohere.com` → **"Sign up"** (Google/GitHub/e-mail).
2. Menu lateral → **"API Keys"**. Uma **Trial key** já vem criada; copia ela ou **"+ New Trial key"**.
3. `.env`: `COHERE_API_KEY=`
4. Modelos: `command-a-03-2025` (geral), `command-a-reasoning-08-2025`. Uso no OpenKax: `kax/reviewer` (família diferente dos autores).

## 7. SambaNova Cloud (3 min)
Endpoint `https://api.sambanova.ai/v1`. 20 RPM, **20 RPD por modelo**, 200k TPD.

1. `cloud.sambanova.ai` → **"Sign up"** (Google/GitHub/e-mail).
2. Menu lateral → **"API Keys"** → **"Generate New API Key"** → copia → `.env`: `SAMBANOVA_API_KEY=`
3. **Não** adiciona método de pagamento — isso te move pro Developer tier (US$5 que expiram em 3 meses, depois cobra).
4. Modelos: `Meta-Llama-3.3-70B-Instruct`, `DeepSeek-V3.2`, `gpt-oss-120b`, `gemma-4-31B-it` — cada um com seus 20/dia.

## 8. Alibaba Model Studio — Qwen (10 min, 90 dias)
Endpoint `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` (**Singapura**; Beijing e Virginia cobram desde o primeiro token).

1. `alibabacloud.com` → **"Free Account"** / "Sign up" → conta internacional por e-mail. Pode pedir **cartão pra verificação** com hold < US$1 — se pedir e você não quiser, pula este provider.
2. `modelstudio.console.alibabacloud.com` → seletor de região no topo → **"Singapore"**.
3. Lê e aceita o ToS → ativação automática + quota free concedida (**o relógio de 90 dias começa agora**).
4. Menu → **"API Key"** → **"Create API Key"** (geral, pay-as-you-go — não a de "Coding Plan"). Copia → `.env`: `DASHSCOPE_API_KEY=`
5. **Obrigatório:** pra cada modelo que for usar (`qwen3-coder-plus`, `qwen3.x-coder-flash`…), abre a **página de detalhes do modelo** (região Singapura) → liga **"Free Quota Only"**. Sem isso, quota acabou = cobrança.
6. Se o console mostrar "Not enabled / Enabling not supported" no switch, é estado de proteção, não erro.

---

## Depois de tudo
```bash
cd ~/openkax && ./doctor.sh
```
Cada provider novo deve sair `[OK]`. `403` com "network/region" = bloqueio da rede do notebook (como o Groq), não da chave — testa do PC de casa.

## Rotação
Todas as chaves que passaram por chat devem ser rotacionadas ao fechar a Fase 0. As criadas por este tutorial foram direto pro `.env` e não precisam.
