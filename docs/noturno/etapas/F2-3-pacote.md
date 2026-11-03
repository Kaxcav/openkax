# Etapa F2-3 — montagem do pacote enxuto

Objetivo: transformar `TaskPacket` + arquivos do worktree em uma `ChatCompletionRequest` pequena para o operário.

Entregáveis:
1. `src/mcp/packet.rs` (ou equivalente): system prompt curto de operário (fixo, versionado no código, PT-BR ou EN — escolha e documente no ADR 0007), seguido de: objetivo, conteúdo dos arquivos `read` (com path), arquivos `write` existentes (conteúdo atual) ou "novo arquivo", contratos, restrições, critério de aceite e formato de saída exigido (`unified_diff` com paths relativos ao repo, ou arquivo completo delimitado).
2. Anexo opcional `CONVENTIONS.md` do repo alvo se existir (curto; truncar com aviso).
3. Estimativa de tokens pela mesma função do scheduler; recusa com erro estruturado se passar de `max_packet_tokens` da classe (sem chamar provider).
4. Parser da resposta do operário: extrai o diff unificado (aceita bloco cercado por ```diff) ou os arquivos completos; rejeita diff que toca arquivo fora da lista `write`, path absoluto ou `..`.
5. Testes de tabela: pacote com 2 reads + 1 write cabe; pacote grande → erro de tamanho; parser aceita diff válido, recusa diff que escreve fora do escopo, recusa path traversal, lida com CRLF.
