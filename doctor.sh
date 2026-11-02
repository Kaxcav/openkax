#!/usr/bin/env bash
# doctor.sh — valida as chaves do .env com um GET /models por provider.
# Uso: ./doctor.sh   (na raiz do repo, com .env preenchido). Não imprime chaves.
# Protótipo do futuro `openkax doctor`. Equivalente Linux/WSL do doctor.ps1.

set -u
cd "$(dirname "$0")"

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
  CODE=$(curl -sS -o "$tmpdir/$name.json" -w '%{http_code}' --max-time 25 "$@" "$url" 2>/dev/null || echo 000)
  if [[ "$CODE" == 2* ]]; then
    printf '\e[32m[OK]   %-11s HTTP %s\e[0m\n' "$name" "$CODE"
  else
    printf '\e[31m[FAIL] %-11s HTTP %s  %s\e[0m\n' "$name" "$CODE" "$(head -c 160 "$tmpdir/$name.json" 2>/dev/null | tr -d '\n')"
    if [[ "$CODE" == 403 ]] && grep -qi "network" "$tmpdir/$name.json" 2>/dev/null; then
      echo -e "       \e[33mbloqueio de rede/região pelo provider — não é a chave. Teste em outra rede (hotspot do celular, PC de casa).\e[0m"
    fi
  fi
}

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

echo
