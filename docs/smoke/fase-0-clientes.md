# Roteiro opcional: Continue e Cline de verdade (F0-12)

Não bloqueia a Fase 0 (ADR 0004). Serve para conferir o que `tests/client_compat.rs` não prova, porque parte dos formatos foi escrita sem ler o `openai.ts` dos clientes (ver o cabeçalho do teste).

## Preparação

1. Suba o gateway e confira `curl -s localhost:8000/v1/models`.
2. Deixe `curl -s localhost:8000/quota` à mão para comparar antes e depois.

## Continue

Modelo em `config.yaml`:

```yaml
models:
  - name: Kax Smart
    provider: openai
    apiBase: http://localhost:8000/v1
    apiKey: local
    model: kax/coder-smart
    capabilities:
      - tool_use
    roles:
      - chat
      - edit
      - apply
```

| Passo | Esperado |
|---|---|
| Chat com histórico de 2 ou mais rodadas | texto em streaming, sem corte |
| Edit de um trecho | diff aplicável |
| Apply de um bloco | arquivo alterado |
| Agente com uma tool | tool_call executada e resposta depois dela |

## Cline

Provider **OpenAI Compatible**: Base URL `http://localhost:8000/v1`, API key qualquer, Model ID `kax/coder-smart`.

| Passo | Esperado |
|---|---|
| Tarefa que leia um arquivo e escreva outro | várias rodadas de tool_calls até concluir |
| Trocar o Model ID para `kax/coder-fast` | erro 413 visível (o prompt do Cline não cabe em 6k tokens) |

## O que anotar

- Campos da request que a suíte não modela. Se aparecer algum, acrescente ao teste.
- Em `/quota`, se `served_by_tier` e os saldos batem com as requests feitas.
- No ledger do dia, se há uma linha por request, sem corpo.
