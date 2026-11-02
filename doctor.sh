#!/usr/bin/env bash
# doctor.sh — valida as chaves do .env com uma chamada mínima por provider.
# Uso: ./doctor.sh   (na raiz do repo, com .env preenchido). Não imprime chaves.
# Provider sem variável no .env é pulado. Protótipo do futuro `openkax doctor`.

set -u
cd "$(dirname "$0")"

if [[ ! -f .env ]]; then
  echo -e "\e[31mSem .env. Rode: cp .env.example .env && nano .env\e[0m"; exit 1
fi

while IFS= read -r line || [[ -n "$line" ]]; do
  [[ "$line" =~ ^[[:space:]]*# ]] && continue
  [[ "$line" =~ ^[[:space:]]*([A-Z0-9_]+)[[:space:]]*=[[:space:]]*(.*)$ ]] || continue
  export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
done < .env

tmpdir=$(mktemp -d); trap 'rm -rf "$tmpdir"' EXIT
CODE=000

# GET simples. req NOME URL [args do curl...] -> $CODE, $tmpdir/NOME.json
req() {
  local name=$1 url=$2; shift 2
  CODE=$(curl -sS -o "$tmpdir/$name.json" -w '%{http_code}' --max-time 25 "$@" "$url" 2>/dev/null || echo 000)
  report "$name"
}

# Ping via chat completion de 1 token (para providers sem /models). ping NOME URL MODELO [args do curl...]
ping() {
  local name=$1 url=$2 model=$3; shift 3
  CODE=$(curl -sS -o "$tmpdir/$name.json" -w '%{http_code}' --max-time 40 -H "Content-Type: application/json" "$@" \
    -d "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":1,\"stream\":false}" \
    "$url" 2>/dev/null || echo 000)
  report "$name"
}

report() {
  local name=$1
  if [[ "$CODE" == 2* ]]; then
    printf '\e[32m[OK]   %-11s HTTP %s\e[0m\n' "$name" "$CODE"
  else
    printf '\e[31m[FAIL] %-11s HTTP %s  %s\e[0m\n' "$name" "$CODE" "$(head -c 160 "$tmpdir/$name.json" 2>/dev/null | tr -d '\n')"
    if [[ "$CODE" == 403 ]] && grep -qi "network\|region\|country" "$tmpdir/$name.json" 2>/dev/null; then
      echo -e "       \e[33mbloqueio de rede/região pelo provider — não é a chave. Teste em outra rede.\e[0m"
    fi
  fi
}

count_models() {
  python3 - "$1" <<'PY'
import sys, json
try:
    d = json.load(open(sys.argv[1]))
    if isinstance(d, dict):
        for k in ("data", "models", "result"):
            if isinstance(d.get(k), list):
                d = d[k]; break
    print(len(d) if isinstance(d, list) else "?")
except Exception:
    print("?")
PY
}

openrouter_quota() {
  python3 - "$1" <<'PY'
import sys, json
try:
    d = json.load(open(sys.argv[1]))["data"]
    f = d.get("free_model_daily_requests") or {}
    lim = f.get("limit")
    print(f"       free_model_daily_requests: limit={lim} used={f.get('used')} remaining={f.get('remaining')}")
    print("       \033[32mdesbloqueio US$10 confirmado (1000 RPD)\033[0m" if (lim or 0) >= 1000
          else "       \033[33mainda em 50 RPD — crédito de US$10 não reconhecido ainda\033[0m")
except Exception:
    print("       não consegui ler free_model_daily_requests")
PY
}

models_line() { [[ "$CODE" == 2* ]] && echo "       modelos visíveis: $(count_models "$tmpdir/$1.json")${2:-}"; }
skip() { printf '\e[90m[SKIP] %-11s sem %s\e[0m\n' "$1" "$2"; }

echo; echo "OpenKax doctor — $(date '+%Y-%m-%d %H:%M')"; echo

# ---------- Tier 1: espinha dorsal ----------
if [[ -n "${GEMINI_API_KEY:-}" ]]; then
  req gemini https://generativelanguage.googleapis.com/v1beta/openai/models -H "Authorization: Bearer $GEMINI_API_KEY"; models_line gemini "  (OpenAI-compat)"
else skip gemini GEMINI_API_KEY; fi

if [[ -n "${MISTRAL_API_KEY:-}" ]]; then
  req mistral https://api.mistral.ai/v1/models -H "Authorization: Bearer $MISTRAL_API_KEY"; models_line mistral
else skip mistral MISTRAL_API_KEY; fi

if [[ -n "${CODESTRAL_API_KEY:-}" ]]; then
  ping codestral https://codestral.mistral.ai/v1/chat/completions codestral-latest -H "Authorization: Bearer $CODESTRAL_API_KEY"
else skip codestral CODESTRAL_API_KEY; fi

if [[ -n "${NVIDIA_API_KEY:-}" ]]; then
  req nvidia https://integrate.api.nvidia.com/v1/models -H "Authorization: Bearer $NVIDIA_API_KEY"; models_line nvidia
else skip nvidia NVIDIA_API_KEY; fi

# ---------- Tier 2: backups free ----------
if [[ -n "${GROQ_API_KEY:-}" ]]; then
  req groq https://api.groq.com/openai/v1/models -H "Authorization: Bearer $GROQ_API_KEY"; models_line groq
else skip groq GROQ_API_KEY; fi

if [[ -n "${GITHUB_MODELS_TOKEN:-}" ]]; then
  req github https://models.github.ai/catalog/models -H "Authorization: Bearer $GITHUB_MODELS_TOKEN" -H "Accept: application/vnd.github+json"; models_line github
else skip github GITHUB_MODELS_TOKEN; fi

if [[ -n "${ZAI_API_KEY:-}" ]]; then
  ping zai https://api.z.ai/api/paas/v4/chat/completions glm-4.7-flash -H "Authorization: Bearer $ZAI_API_KEY"
else skip zai ZAI_API_KEY; fi

if [[ -n "${CLOUDFLARE_API_TOKEN:-}" && -n "${CLOUDFLARE_ACCOUNT_ID:-}" ]]; then
  req cloudflare "https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/ai/models/search?per_page=100" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN"; models_line cloudflare
else skip cloudflare "CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID"; fi

if [[ -n "${SAMBANOVA_API_KEY:-}" ]]; then
  req sambanova https://api.sambanova.ai/v1/models -H "Authorization: Bearer $SAMBANOVA_API_KEY"; models_line sambanova
else skip sambanova SAMBANOVA_API_KEY; fi

if [[ -n "${COHERE_API_KEY:-}" ]]; then
  req cohere https://api.cohere.com/v1/models -H "Authorization: Bearer $COHERE_API_KEY"; models_line cohere
else skip cohere COHERE_API_KEY; fi

if [[ -n "${DASHSCOPE_API_KEY:-}" ]]; then
  req qwen https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models -H "Authorization: Bearer $DASHSCOPE_API_KEY"; models_line qwen
else skip qwen DASHSCOPE_API_KEY; fi

# ---------- Tier 3: agregadores e pagos ----------
if [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
  req openrouter https://openrouter.ai/api/v1/key -H "Authorization: Bearer $OPENROUTER_API_KEY"
  [[ "$CODE" == 2* ]] && openrouter_quota "$tmpdir/openrouter.json"
else skip openrouter OPENROUTER_API_KEY; fi

if [[ -n "${VERCEL_AI_GATEWAY_KEY:-}" ]]; then
  req vercel https://ai-gateway.vercel.sh/v1/models -H "Authorization: Bearer $VERCEL_AI_GATEWAY_KEY"; models_line vercel
else skip vercel VERCEL_AI_GATEWAY_KEY; fi

if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  req deepseek https://api.deepseek.com/v1/models -H "Authorization: Bearer $DEEPSEEK_API_KEY"; models_line deepseek
else skip deepseek DEEPSEEK_API_KEY; fi

echo
