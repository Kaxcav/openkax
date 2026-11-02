#!/usr/bin/env bash
# doctor.sh — valida as chaves do .env com um GET /models por provider.
# Uso: ./doctor.sh   (na raiz do repo, com .env preenchido). Não imprime chaves.
# Protótipo do futuro `openkax doctor`. Equivalente Linux/WSL do doctor.ps1.

set -u
cd "$(dirname "$0")"

if [[ ! -f .env ]]; then
  echo -e "\e[31mSem .env. Rode: cp .env.example .env && nano .env\e[0m"; exit 1
fi

# carrega .env sem exportar lixo
while IFS= read -r line; do
  [[ "$line" =~ ^[[:space:]]*# ]] && continue
  [[ "$line" =~ ^[[:space:]]*([A-Z0-9_]+)[[:space:]]*=[[:space:]]*(.*)$ ]] || continue
  export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
done < .env

have_py=0; command -v python3 >/dev/null && have_py=1

count_models() {  # lê stdin JSON, imprime .data | length (ou "?")
  if (( have_py )); then python3 -c 'import sys,json
try:
  d=json.load(sys.stdin); d=d.get("data",d); print(len(d))
except Exception: print("?")'; else echo "?"; fi
}

check() {  # check NOME URL [header...]
  local name=$1 url=$2; shift 2
  local body code
  body=$(mktemp)
  code=$(curl -sS -o "$body" -w '%{http_code}' --max-time 25 "$@" "$url" 2>/dev/null || echo 000)
  if [[ "$code" == 2* ]]; then
    printf '\e[32m[OK]   %-11s HTTP %s\e[0m' "$name" "$code"
  else
    printf '\e[31m[FAIL] %-11s HTTP %s  %s\e[0m' "$name" "$code" "$(head -c 160 "$body" | tr -d '\n')"
  fi
  echo
  cat "$body"; rm -f "$body"
}

echo; echo "OpenKax doctor — $(date '+%Y-%m-%d %H:%M')"; echo

if [[ -n "${GEMINI_API_KEY:-}" ]]; then
  out=$(check gemini https://generativelanguage.googleapis.com/v1beta/openai/models -H "Authorization: Bearer $GEMINI_API_KEY")
  echo "$out" | head -1; echo "       modelos visíveis: $(echo "$out" | tail -n +2 | count_models)  (endpoint OpenAI-compat)"
else echo -e "\e[90m[SKIP] gemini      sem GEMINI_API_KEY\e[0m"; fi

if [[ -n "${MISTRAL_API_KEY:-}" ]]; then
  out=$(check mistral https://api.mistral.ai/v1/models -H "Authorization: Bearer $MISTRAL_API_KEY")
  echo "$out" | head -1; echo "       modelos visíveis: $(echo "$out" | tail -n +2 | count_models)"
else echo -e "\e[90m[SKIP] mistral     sem MISTRAL_API_KEY\e[0m"; fi

if [[ -n "${GROQ_API_KEY:-}" ]]; then
  out=$(check groq https://api.groq.com/openai/v1/models -H "Authorization: Bearer $GROQ_API_KEY")
  echo "$out" | head -1; echo "       modelos visíveis: $(echo "$out" | tail -n +2 | count_models)"
else echo -e "\e[90m[SKIP] groq        sem GROQ_API_KEY\e[0m"; fi

if [[ -n "${GITHUB_MODELS_TOKEN:-}" ]]; then
  out=$(check github https://models.github.ai/catalog/models -H "Authorization: Bearer $GITHUB_MODELS_TOKEN" -H "Accept: application/vnd.github+json")
  echo "$out" | head -1; echo "       modelos no catálogo: $(echo "$out" | tail -n +2 | count_models)"
else echo -e "\e[90m[SKIP] github      sem GITHUB_MODELS_TOKEN\e[0m"; fi

if [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
  out=$(check openrouter https://openrouter.ai/api/v1/key -H "Authorization: Bearer $OPENROUTER_API_KEY")
  echo "$out" | head -1
  if (( have_py )); then
    echo "$out" | tail -n +2 | python3 -c 'import sys,json
try:
  f=json.load(sys.stdin)["data"].get("free_model_daily_requests") or {}
  lim=f.get("limit"); print(f"       free_model_daily_requests: limit={lim} used={f.get(\"used\")} remaining={f.get(\"remaining\")}")
  print("       \033[32mdesbloqueio US$10 confirmado (1000 RPD)\033[0m" if (lim or 0)>=1000 else "       \033[33mainda em 50 RPD — crédito de US$10 não reconhecido ainda\033[0m")
except Exception as e: print("       não consegui ler free_model_daily_requests")'
  fi
else echo -e "\e[90m[SKIP] openrouter  sem OPENROUTER_API_KEY\e[0m"; fi

if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  out=$(check deepseek https://api.deepseek.com/v1/models -H "Authorization: Bearer $DEEPSEEK_API_KEY")
  echo "$out" | head -1
else echo -e "\e[90m[SKIP] deepseek    sem DEEPSEEK_API_KEY\e[0m"; fi

echo
