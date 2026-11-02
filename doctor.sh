#!/usr/bin/env bash
# doctor.sh — valida as chaves do .env com um GET /models por provider.
# Uso: ./doctor.sh [--chat p1,p2,...] [--save DIR]
#   --chat  faz também 1 chat mínimo (max_tokens=1) nos providers listados, para
#           capturar headers x-ratelimit-* reais (gasta 1 request de cota de cada).
#   --save  grava os JSON de /models em DIR (para conferir IDs upstream).
# Cloudflare e Cohere não têm /models OpenAI-compat: sempre fazem o chat mínimo.
# Nunca imprime chaves; dos headers de resposta só mostra x-ratelimit-*, retry-after e
# similares. Protótipo do futuro `openkax doctor`. Equivalente Linux/WSL do doctor.ps1.

set -u
cd "$(dirname "$0")"

CHAT=","; SAVE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --chat) CHAT=",$2,"; shift 2 ;;
    --save) SAVE="$2"; mkdir -p "$SAVE"; shift 2 ;;
    *) echo "uso: $0 [--chat p1,p2] [--save DIR]"; exit 2 ;;
  esac
done

if [[ ! -f .env ]]; then
  echo -e "\e[31mSem .env. Rode: cp .env.example .env && nano .env\e[0m"; exit 1
fi

# carrega .env (ignora comentários e linhas inválidas)
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ "$line" =~ ^[[:space:]]*# ]] && continue
  [[ "$line" =~ ^[[:space:]]*([A-Z0-9_]+)[[:space:]]*=[[:space:]]*(.*)$ ]] || continue
  export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
done < .env

tmpdir=$(mktemp -d); trap 'rm -rf "$tmpdir"' EXIT
CODE=000

req() {  # req NOME URL [args do curl...]  -> define $CODE e grava $tmpdir/NOME.json
  local name=$1 url=$2; shift 2
  CODE=$(curl -sS -o "$tmpdir/$name.json" -D "$tmpdir/$name.hdr" -w '%{http_code}' --max-time 25 "$@" "$url" 2>/dev/null || echo 000)
  if [[ "$CODE" == 2* ]]; then
    printf '\e[32m[OK]   %-11s HTTP %s\e[0m\n' "$name" "$CODE"
  else
    printf '\e[31m[FAIL] %-11s HTTP %s  %s\e[0m\n' "$name" "$CODE" "$(head -c 160 "$tmpdir/$name.json" 2>/dev/null | tr -d '\n')"
    if [[ "$CODE" == 403 ]] && grep -qi "network" "$tmpdir/$name.json" 2>/dev/null; then
      echo -e "       \e[33mbloqueio de rede/região pelo provider — não é a chave. Teste em outra rede (hotspot do celular, PC de casa).\e[0m"
    fi
  fi
  ratelimit_headers "$tmpdir/$name.hdr"
  [[ -n "$SAVE" && "$CODE" == 2* ]] && cp "$tmpdir/$name.json" "$SAVE/$name.json"
}

ratelimit_headers() {  # só headers de limite; nunca auth/cookies
  [[ -f "$1" ]] || return 0
  grep -iE '^(x-ratelimit-[a-z0-9-]+|ratelimit[a-z-]*|retry-after|x-request-limit[a-z-]*|cf-[a-z-]*limit[a-z-]*):' "$1" \
    | tr -d '\r' | sed 's/^/       │ /'
}

chat() {  # chat NOME URL MODELO [args do curl...] → POST /chat/completions mínimo
  local name=$1 url=$2 model=$3; shift 3
  req "$name-chat" "$url/chat/completions" -X POST -H 'Content-Type: application/json' "$@" \
    -d "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":1}"
}

wants_chat() { [[ "$CHAT" == *",$1,"* || "$CHAT" == *",all,"* ]]; }

count_models() {  # count_models ARQUIVO.json
  python3 - "$1" <<'PY'
import sys, json
try:
    d = json.load(open(sys.argv[1]))
    if isinstance(d, dict):
        d = d.get("data", d.get("models", []))
    print(len(d) if isinstance(d, list) else "?")
except Exception:
    print("?")
PY
}

openrouter_quota() {  # openrouter_quota ARQUIVO.json
  python3 - "$1" <<'PY'
import sys, json
try:
    d = json.load(open(sys.argv[1]))["data"]
    f = d.get("free_model_daily_requests") or {}
    lim = f.get("limit")
    print(f"       free_model_daily_requests: limit={lim} used={f.get('used')} remaining={f.get('remaining')}")
    if (lim or 0) >= 1000:
        print("       \033[32mdesbloqueio US$10 confirmado (1000 RPD)\033[0m")
    else:
        print("       \033[33mainda em 50 RPD — crédito de US$10 não reconhecido ainda\033[0m")
    rl = d.get("rate_limit") or {}
    if rl:
        print(f"       rate_limit: {rl.get('requests')} req / {rl.get('interval')}")
except Exception:
    print("       não consegui ler free_model_daily_requests")
PY
}

echo; echo "OpenKax doctor — $(date '+%Y-%m-%d %H:%M')"; echo

if [[ -n "${GEMINI_API_KEY:-}" ]]; then
  req gemini https://generativelanguage.googleapis.com/v1beta/openai/models -H "Authorization: Bearer $GEMINI_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/gemini.json")  (endpoint OpenAI-compat)"
else echo -e "\e[90m[SKIP] gemini      sem GEMINI_API_KEY\e[0m"; fi

if [[ -n "${MISTRAL_API_KEY:-}" ]]; then
  req mistral https://api.mistral.ai/v1/models -H "Authorization: Bearer $MISTRAL_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/mistral.json")"
else echo -e "\e[90m[SKIP] mistral     sem MISTRAL_API_KEY\e[0m"; fi

if [[ -n "${GROQ_API_KEY:-}" ]]; then
  req groq https://api.groq.com/openai/v1/models -H "Authorization: Bearer $GROQ_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/groq.json")"
else echo -e "\e[90m[SKIP] groq        sem GROQ_API_KEY\e[0m"; fi

if [[ -n "${GITHUB_MODELS_TOKEN:-}" ]]; then
  req github https://models.github.ai/catalog/models -H "Authorization: Bearer $GITHUB_MODELS_TOKEN" -H "Accept: application/vnd.github+json"
  [[ "$CODE" == 2* ]] && echo "       modelos no catálogo: $(count_models "$tmpdir/github.json")"
else echo -e "\e[90m[SKIP] github      sem GITHUB_MODELS_TOKEN\e[0m"; fi

if [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
  req openrouter https://openrouter.ai/api/v1/key -H "Authorization: Bearer $OPENROUTER_API_KEY"
  [[ "$CODE" == 2* ]] && openrouter_quota "$tmpdir/openrouter.json"
else echo -e "\e[90m[SKIP] openrouter  sem OPENROUTER_API_KEY\e[0m"; fi

if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  req deepseek https://api.deepseek.com/v1/models -H "Authorization: Bearer $DEEPSEEK_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/deepseek.json")"
else echo -e "\e[90m[SKIP] deepseek    sem DEEPSEEK_API_KEY\e[0m"; fi


# --------------------------------------------------------- providers de cauda (ADR 0004)

if [[ -n "${NVIDIA_API_KEY:-}" ]]; then
  req nvidia https://integrate.api.nvidia.com/v1/models -H "Authorization: Bearer $NVIDIA_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/nvidia.json")"
  wants_chat nvidia && chat nvidia https://integrate.api.nvidia.com/v1 "${NVIDIA_DOCTOR_MODEL:-meta/llama-3.1-8b-instruct}" -H "Authorization: Bearer $NVIDIA_API_KEY"
else echo -e "\e[90m[SKIP] nvidia      sem NVIDIA_API_KEY\e[0m"; fi

# /v1/models do AI Gateway é público; a chave só é exigida no chat.
req vercel https://ai-gateway.vercel.sh/v1/models
[[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/vercel.json")"
if [[ -n "${VERCEL_AI_GATEWAY_KEY:-}" ]]; then
  wants_chat vercel && chat vercel https://ai-gateway.vercel.sh/v1 "${VERCEL_DOCTOR_MODEL:-openai/gpt-oss-20b}" -H "Authorization: Bearer $VERCEL_AI_GATEWAY_KEY"
else echo -e "\e[90m       sem VERCEL_AI_GATEWAY_KEY: chat não testado\e[0m"; fi

if [[ -n "${ZAI_API_KEY:-}" ]]; then
  req zai https://api.z.ai/api/paas/v4/models -H "Authorization: Bearer $ZAI_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/zai.json")"
  wants_chat zai && chat zai https://api.z.ai/api/paas/v4 "${ZAI_DOCTOR_MODEL:-glm-4.7-flash}" -H "Authorization: Bearer $ZAI_API_KEY"
else echo -e "\e[90m[SKIP] zai         sem ZAI_API_KEY\e[0m"; fi

if [[ -n "${CLOUDFLARE_ACCOUNT_ID:-}" && -n "${CLOUDFLARE_API_TOKEN:-}" ]]; then
  cf=https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/ai
  req cloudflare "$cf/models/search?task=Text%20Generation&per_page=100" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN"
  [[ "$CODE" == 2* ]] && echo "       modelos de texto: $(python3 -I -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("result",[])))' "$tmpdir/cloudflare.json" 2>/dev/null || echo '?')"
  chat cloudflare "$cf/v1" "${CLOUDFLARE_DOCTOR_MODEL:-@cf/meta/llama-3.1-8b-instruct}" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN"
else echo -e "\e[90m[SKIP] cloudflare  precisa de CLOUDFLARE_ACCOUNT_ID e CLOUDFLARE_API_TOKEN\e[0m"; fi

if [[ -n "${COHERE_API_KEY:-}" ]]; then
  # Lista pela API nativa (a de compatibilidade não documenta /models).
  req cohere "https://api.cohere.com/v1/models?endpoint=chat&page_size=100" -H "Authorization: Bearer $COHERE_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos de chat: $(count_models "$tmpdir/cohere.json")"
  chat cohere https://api.cohere.ai/compatibility/v1 "${COHERE_DOCTOR_MODEL:-command-a-plus-05-2026}" -H "Authorization: Bearer $COHERE_API_KEY"
else echo -e "\e[90m[SKIP] cohere      sem COHERE_API_KEY\e[0m"; fi

if [[ -n "${SAMBANOVA_API_KEY:-}" ]]; then
  req sambanova https://api.sambanova.ai/v1/models -H "Authorization: Bearer $SAMBANOVA_API_KEY"
  [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/sambanova.json")"
  wants_chat sambanova && chat sambanova https://api.sambanova.ai/v1 "${SAMBANOVA_DOCTOR_MODEL:-Meta-Llama-3.3-70B-Instruct}" -H "Authorization: Bearer $SAMBANOVA_API_KEY"
else echo -e "\e[90m[SKIP] sambanova   sem SAMBANOVA_API_KEY\e[0m"; fi

echo
