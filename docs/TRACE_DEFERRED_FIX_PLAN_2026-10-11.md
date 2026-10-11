**Plano de correção e campanha de testes — BUG-001**

RUN-20261011-DEFERRED-001. O usuário delegou a implementação com “corrija” após o planejamento. O registro pré-fix abaixo foi preservado como evidência histórica; a implementação e sua validação independente estão consolidadas na seção pós-fix ao final. O protocolo do código corrigido é V17, com formato de estado 1.

**Resultado e cenário**

Snapshot: branch `main`, commit `9c5db90b68343522ad0c5280035da390cab5dc73`, árvore inicialmente limpa. Linux 7.0.0-28-generic/x86_64; Rust e Cargo 1.98.1. Protocolo atual 16; formato de estado 1. Testes funcionais Rust em debug. Android usa a variante release para os testes JVM, com JDK 17 e SDK 35 locais; isso não transforma os testes Rust em benchmarks release.

Os testes usam diretórios temporários, arquivos artificiais e pares de sockets Unix. O Store de teste usa o LocalStore real e injeta `ScanDeferred` uma única vez em um método específico, reproduzindo o resultado tipado que a ponte JNI produz ao receber `AUDIT_DEFERRED`. Não há sleeps para acertar a race. Esse método testa o protocolo e a persistência reais, mas não reproduz a implementação de um provedor SAF físico, Wi-Fi ou o serviço Android completo.

**Resultado: FAIL pré-fix confirmado em REG-001–004.** Os quatro contratos alcançaram a injeção e falharam exatamente na exigência de uma rodada adiada em vez de erro fatal. Frequência nesta campanha: 1/1 em cada cenário; não houve repetição para estimar flakiness do aplicativo físico. O controle positivo de `HashNext`, a retomada na mesma conexão e o smoke nas três direções passaram. Nenhuma correção de produção foi aplicada; o gate pós-fix permanece não executado.

**Evidências da execução**

| Cenário | Resultado | Pedidos enviados / consumidos pelo responder |
|---|---|---|
| REG-001, início do hashing | FAIL esperado, `ShareError`, operation `HashStreamBegin` | `HashNext` enviado depois do begin e não consumido. |
| REG-002, primeiro Get do lote | FAIL esperado, `ShareError`, operation `snapshot` | Quatro Gets enviados, um consumido; três pendentes. |
| REG-003, segundo Get do lote | FAIL esperado, `ShareError`, operation `snapshot` | Quatro Gets enviados, dois consumidos; dois pendentes. O primeiro Blob já tinha sido recebido corretamente. |
| REG-004, arquivo de 9 MiB | FAIL esperado, `ShareError`, operation `snapshot` | Um Get enviado e consumido; demonstra falha também sem janela múltipla. |

Antes de cada falha de contrato, as verificações de base em memória/disco, ausência de `state.next`, preservação do conteúdo anterior e ausência de publicação parcial passaram. A mensagem do PC foi `peer: responder ... audit deferred`; o responder devolveu o erro tipado `ScanDeferred`. Como o fixture roda no Linux, o campo side do erro é `responder`, não `android`.

O experimento discrimina a classificação de cancelamento de erro real de conteúdo/transport: a rodada inicial funciona, a entrada é artificial e válida, o único fault injetado é o resultado tipado retornado pela JNI. Os bytes registrados nas duas extremidades demonstram o estado dos requests pendentes. A origem completa do wake/SAF continua sustentada pelos traces e pelos unitários de WakeState, não por uma reprodução física nessa campanha.

Baseline: `cargo check --workspace --all-features --locked` passou; 142 unitários Rust passaram, com três benchmarks ignorados. `testReleaseUnitTest` passou com 65 testes JVM, zero falhas/erros/skips, incluindo sete WakeStateTest e os testes de digest na JNI real de host. A contagem das integrações está registrada ao final deste documento. A tentativa inicial de Rust/Gradle no sandbox encontrou restrições de sockets/cache; as execuções fora dele passaram pelas aprovações do ambiente. Isso não foi tratado como falha do Rowd.

**Problema e causa**

BUG-001, severidade média: um adiamento deliberado em `HashStreamBegin` ou `Get/snapshot` aborta a rodada com `ShareError` não reutilizável. Os traces mostram recovery automático, mas também recriação desnecessária da conexão e mensagem de falha. O caminho `HashNext` já converte o mesmo erro em uma rodada adiada.

O namespace SAF captura a geração da share. Um wake manual global incrementa a geração; `FolderAccess.checkStream()` invalida o namespace e lança `AuditDeferred`. A JNI converte isso em `sync::ScanDeferred`. No responder, `store.start_hash_stream()` e `store.snapshot()` propagam o erro ao tratamento genérico. Esse tratamento envia `ShareError` com `stream_reusable=false`. A diferença de tratamento em relação a `next_hash_chunk()` é observável no código e nos traces.

Há duas restrições de framing que impedem uma troca local de erro por sucesso:

- `HashStreamBegin` não tem resposta própria. O PC envia `HashNext` depois dele. Retornar do responder ao falhar o begin pode deixar esse `HashNext` para o dispatcher da sessão seguinte.
- Até quatro `Get` podem estar enviados antes da leitura dos respectivos Blobs. Retornar no primeiro snapshot adiado pode deixar os demais `Get` na conexão. O PC precisa reconhecer e consumir as respostas de todos os pedidos enviados antes de passar à próxima rodada.

O estado em memória do coordenador já é uma candidata: `coordinate_with_progress_and_audit_control()` só a promove se o relatório não for adiado. Isso deve ser reaproveitado. Não atualizar a base confirmada no tratamento de cancelamento.

**Invariantes**

| ID | Critério |
|---|---|
| INV-001 | Adiamento deliberado antes do payload retorna `round_deferred=true`, sem erro fatal de share. |
| INV-002 | Uma rodada nova começa com o próximo frame esperado, sem requests/responses sobrando. |
| INV-003 | A base confirmada em memória e disco não muda durante uma rodada adiada; não sobra `state.next`. |
| INV-004 | Só conteúdo inteiro e verificado pode aparecer no destino; staging parcial é descartado. |
| INV-005 | Hash diferente, binding inválido, erro real de I/O e frame incompleto continuam sendo erros reais. |
| INV-006 | Manter a janela de quatro arquivos/8 MiB e o caminho serial para arquivos maiores. |
| INV-007 | A geração de mudanças, os wakes pendentes e as restrições de direção continuam valendo. |

**MOD-001 — mecanismo recomendado para o início do hashing**

Separar “hashing autorizado” de “worker iniciado”. Se `start_hash_stream()` retornar especificamente `ScanDeferred`, cancelar/juntar o worker e marcar o scan como adiado. Não emitir uma resposta espontânea ao frame sem resposta `HashStreamBegin`.

Ao receber o próximo `HashNext` válido para aquele scan/sequence, emitir o `ScanDeferred` que ele já aceita como resposta e retornar um relatório adiado. Se o próximo controle for `AuditPreempt`, usar o encerramento existente de preempção. Requests de outro scan/share, saltos de sequência ou mensagens inesperadas continuam sendo rejeitados. Não relaxar a validação do binding nem iniciar hashing com namespace invalidado.

Essa estratégia evita um novo tipo de frame e resolve o request que o PC já enviou. A implementação pode escolher um pequeno estado explícito do responder ou um marcador local; o critério é a ordem de mensagens, não uma abstração específica.

**MOD-002 — mecanismo recomendado para snapshots e a janela de Get**

No responder, um `ScanDeferred` de snapshot antes de enviar `Blob` ativa um estado de adiamento da janela. Responder ao `Get` corrente com `ScanDeferred`; continuar consumindo os `Get` já em voo e responder a cada um com `ScanDeferred`, sem abrir fontes nem produzir novos payloads. Validar share/path/Entry e limitar esse caminho à janela já permitida pelo protocolo.

No PC, tanto `drain_gets()` quanto `remote_snapshot()` devem aceitar `Blob` ou adiamento tipado. No lote, consumir exatamente uma resposta por `Get` já enviado. Depois do primeiro adiamento, parar de agendar trabalho; descartar os snapshots privados desse lote se a instalação ainda não começou. Instalações de lotes anteriores permanecem fisicamente corretas, sem promover a candidata a base.

Após drenar a janela, o PC envia `AuditPreempt` como barreira. O responder só retorna ao dispatcher depois de consumir essa barreira, cancelar/juntar o worker, descartar staging e responder com o `ScanDeferred` da barreira. O PC consome essa resposta antes de devolver `Report { round_deferred: true, ... }`. Assim os dois lados retomam juntos no limite da share, permitindo `RoundDeferred` e uma nova sessão/rodada na conexão existente.

O estado de adiamento de Get também precisa permitir essa barreira quando já não existe um hash stream ativo, por exemplo em um Get posterior ao fechamento do stream. Não introduzir permissões genéricas para descartar frames desconhecidos. Tratamentos de `ScanDeferred` recebidos onde não existe operação cancelável em andamento continuam sendo erro de protocolo.

Não instalar um primeiro Blob do lote apenas para “salvar progresso” antes de confirmar a drenagem. A alternativa de descartar o staging ainda não publicado é mais simples e conserva INV-003/004. A próxima rodada recupera o progresso já publicado anteriormente a partir dos arquivos físicos.

**Componentes e compatibilidade**

- `crates/rowd-core/src/sync.rs`: responder, leitores de Blob, janela de Get, relatório adiado e candidata de estado.
- `crates/rowd-core/src/protocol.rs`: conferir os contextos de `ScanDeferred`; manter as validações de frames/Blobs. Recomenda-se atualizar 16 → 17, pois passa a existir adiamento como resposta a Get, que um PC antigo não aceita. PC e APK devem ser distribuídos juntos.
- `crates/rowd-core/src/managed.rs` e `crates/rowd-app/src/lib.rs`: verificar dispatcher, `RoundDeferred` e wakes pendentes; alterar somente se os testes de sessão demonstrarem necessidade.
- `crates/rowd-android/src/lib.rs`: manter a conversão JNI para erro tipado; não converter exceções por mensagem no core.
- `android/.../FolderAccess.kt`: ajustar apenas a classificação de trace de `AuditDeferred`, se necessário para não registrar um cancelamento esperado como falha SAF. Não mudar `checkStream()` para ignorar mudanças de geração.

Nenhuma mudança no formato de estado, journal ou base confirmada. Não remover os backups existentes. Não interromper um corpo de Blob para emitir controle: payload incompleto continua exigindo fechamento/recovery do transporte.

**Alternativas consideradas e riscos**

| Alternativa | Decisão |
|---|---|
| Capturar ScanDeferred e retornar imediatamente do responder | Rejeitada: deixa pedidos em voo e pode desalinhá-los com a próxima share. |
| Serializar todos os Get | Rejeitada como correção geral: perde a janela existente e muda a performance do caminho normal. |
| Ignorar wake manual ou conferir menos metadados | Rejeitada: enfraquece invalidation/STALE_SOURCE e pode ocultar edições reais. |
| Criar novos frames de abort/ack | Alternativa válida, porém adiciona protocolo e consumidores. Reavaliar se a barreira existente não puder ter semântica inequívoca nos testes adversariais. |
| ScanDeferred por resposta pendente + barreira de AuditPreempt | Recomendada: mantém uma resposta por request, uma única thread dona do socket e a janela atual. |

Riscos principais: ACK/preempção concorrente, bloqueio ao esperar uma barreira que o peer não envia, abuso de frames após adiamento e retorno antecipado ao dispatcher. Preservar os timeouts já existentes, limite de requests e identificação de share/scan. O retry normal continua idempotente porque usa o estado físico e a base anterior. Um crash no adiamento não pode promover `state.next` nem publicar staging. Testes Linux do core não provam a limpeza de workers/URIs SAF no Android; isso exige validação JVM/JNI e dispositivo.

**Testes pré-fix e critérios de aceite**

Arquivo: `crates/rowd-core/tests/scan_deferred.rs`.

| Teste | Proteção |
|---|---|
| `hash_next_deferral_keeps_base_and_reuses_connection` | Controle positivo: adiamento já suportado, mesma conexão, base preservada, retomada e no-op. |
| `hash_begin_deferral_keeps_base_and_reuses_connection` | REG-001: início de hashing invalidado, incluindo HashNext já enviado. |
| `first_get_deferral_drains_four_requests_and_reuses_connection` | REG-002: quatro Gets em voo; primeiro snapshot adiado. |
| `second_get_deferral_drains_valid_blob_and_later_requests` | REG-003: primeiro Blob válido, segundo snapshot adiado, requests posteriores pendentes. |
| `large_get_deferral_keeps_base_and_reuses_connection` | REG-004: caminho serial acima de 8 MiB. |
| `create_modify_restart_and_direction_smoke` | CREATE/MODIFY/reabertura de Stores e estado; ToPc, ToAndroid e bidirecional; writes proibidos preservados apenas no destino. |

REG-001–004 são contratos de aceite ignorados explicitamente na suíte normal enquanto não existe implementação autorizada. Eles são executados com `--ignored` para registrar o FAIL pré-fix. Retirar os ignores somente depois que a correção os fizer passar. A suíte normal não deve transformar essas falhas conhecidas em uma falsa alegação de correção.

Os contratos armam a falha somente após uma rodada inicial bem-sucedida. Conferem a base em memória e disco, inexistência de `state.next`, integridade de qualquer arquivo já publicado e ausência de payload parcial. Se o adiamento for aceito, a retomada usa os mesmos sockets, compara conteúdo/hash e executa outra rodada com zero transferências. A gravação dos frames enviados e consumidos discrimina requests pendentes de falha de filesystem.

**Casos adversariais pós-fix obrigatórios**

1. Invalidar antes/depois de `HashStreamBegin`, após HashChunk e antes de cada posição 1–4 da janela de Get; testar também a última posição.
2. Receber um Blob correto antes do adiamento e garantir descarte seguro de staging ainda não publicado; preservar lotes já instalados.
3. Cruzar `AuditPreempt` do PC com adiamento local do Android; não duplicar barreiras nem perder wakes de outra share.
4. Reiniciar cada peer antes/depois da barreira e antes de ACK/Done; verificar base anterior, no-op e recuperação por conteúdo físico.
5. Peer sem resposta, EOF durante drenagem, frame truncado, hash/Entry incorretos e request para share/scan errado devem manter erro real e fechamento seguro.
6. Rodada de outra share imediatamente após a adiada pelo dispatcher gerenciado; não aceitar Get/HashNext da share anterior nessa rodada.
7. Validar JNI/SAF real: invalidation de geração por MANUAL e LOCAL_OBSERVER, cancelamento/join do worker, limpeza do staging e serviço capaz de continuar no mesmo transporte.

**Performance e segundo estágio**

A gravação original mostra revalidação sem transferência de 67,442 s, com 53,849 s de hashing no PC. O evento indica `force_full_scan=true` mesmo com cache confiável e sem paths dirty. A origem exata desse flag ainda não está instrumentada; não há autorização causal para desabilitar a auditoria integral ou reutilizar hashes sem guardas.

O plano para esse custo é separado: instrumentar, em um experimento isolado, a origem de invalidação (registro do watcher, auditoria de 15 minutos, overflow/rescan e hints). Comparar release antes/depois com dataset idêntico, medindo hashing, latência até o primeiro Blob e bytes lidos. A correção de adiamento deve conservar janela/limites e não acrescentar leituras de conteúdo no caminho normal. Não comparar os tempos debug desta campanha com os números dos traces como benchmark.

**HANDOFF-001 e próxima ação**

Handoff: BUG-001 → INV-001–007 → MOD-001/002 → REG-001–004. Implementar o mecanismo tipado nos dois peers; preservar validação de geração, framing, persistência e limites. Não mascarar com retry/sleep. Critério de aceite: contratos vermelhos pré-fix passam pós-fix, mesma conexão reutilizada, dispatcher alinhado, base anterior mantida, hashes corretos, erros reais mantidos e nenhuma regressão relevante na suíte relacionada.

Próxima ação principal: **HANDOFF_TO_PONYTAIL**. A reprodução registrada nesta campanha confirma a propagação fatal e os requests pendentes, permitindo entregar uma especificação causal. O handoff não implementa a correção. Quando a implementação for delegada, usar Ponytail e exigir revisão/validação por um agente ou revisor diferente do implementador. O gate pós-fix não foi executado nesta etapa.

**Consolidação da validação**

| Verificação | Resultado |
|---|---|
| Check do workspace, todas as features | PASS |
| Unitários Rust | PASS: 142, três benchmarks ignorados |
| Unitários JVM/JNI de host | PASS: 65, sem falhas/erros/skips |
| Integrações Rust | PASS: 28; incluem os dois testes novos de controle/smoke |
| Contratos pré-fix de BUG-001, execução explícita | FAIL esperado: quatro de quatro cenários, uma execução em cada |
| Formatação do novo teste e whitespace do diff | PASS |
| Pós-fix, benchmark release comparativo e aparelho/SAF real | Não executados nesta etapa |

São **235 testes aprovados**, além dos quatro contratos que reproduzem a falha. Os quatro contratos ficam ignorados somente na invocação normal e foram efetivamente executados na invocação pré-fix. Esses resultados não declaram o bug corrigido nem validam a implementação proposta.

Comandos de reprodução, usando o toolchain local desta máquina:

```bash
export PATH="/home/leite/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
export CARGO_TARGET_DIR=/tmp/rowd-testing-target
cargo check --workspace --all-features --locked
cargo test --workspace --lib --bins --locked --offline
env JAVA_HOME="$PWD/.toolchain/jdk" ANDROID_HOME="$PWD/.toolchain/android-sdk" \
    PATH="$PWD/.toolchain/jdk/bin:$PATH" \
    CARGO_TARGET_DIR="$PWD/target" \
    .toolchain/gradle-8.9/bin/gradle -p android testReleaseUnitTest --offline --no-daemon --console=plain
cargo test --workspace --test '*' --locked --offline
cargo test -p rowd-core --test scan_deferred --locked --offline -- --ignored --nocapture --test-threads=1
```

O último comando retorna 101 no pré-fix, por falhas de contrato esperadas. No pós-fix ele deve retornar zero e os ignores devem ser removidos. Sockets locais e o cache Gradle precisam das permissões normais do host; restrições do sandbox foram verificadas antes de repetir com a aprovação do ambiente.

Logs completos desta execução: `/tmp/rowd-testing-baseline-unit.log`, `/tmp/rowd-testing-baseline-kotlin.log`, `/tmp/rowd-testing-integration.log` e `/tmp/rowd-testing-pre-fix.log`. Contagens consolidadas em `/tmp/rowd-testing-summary.json`. Relatório dos traces originais: `/tmp/rowd-trace-analysis/relatorio.md`. Apenas o teste de integração e este documento foram adicionados ao working tree.

**Implementação e validação pós-fix — 2026-10-11**

Resultado funcional: **PASS**. Implementação autorizada pelo usuário e realizada pelo agente `/root/implement_fix` com a skill Ponytail. Revisão do diff, acréscimo de cenários adversariais e execução final foram feitos independentemente por `/root`, com `rowd-testing`. Referência: mesmo HEAD `9c5db90b68343522ad0c5280035da390cab5dc73`, acrescido das alterações locais descritas abaixo. Não houve commit, instalação em shares reais ou alteração dos traces originais.

A causa confirmada foi corrigida em `sync.rs`: o erro tipado do início do hashing fica associado ao scan e só é respondido no próximo `HashNext` válido ou `AuditPreempt`. Uploads privados do PC que antecedem esse controle conservam validação e limites, e são descartados ao adiar. No snapshot, o responder entra no estado de Get adiado, responde uma vez por pedido pendente e só termina após a barreira `AuditPreempt`. Os dois leitores de Blob do PC reconhecem esse resultado; a janela é drenada antes de descartar o lote privado e devolver `round_deferred=true`.

Não há retry/sleep novo, serialização da janela nem promoção da candidata a base. Lotes já publicados permanecem íntegros e a retomada transfere apenas os arquivos restantes. A revisão identificou também headers de payload inesperados depois do adiamento; agora recebem `IncompleteFrame`, impedindo controles sobre corpo não consumido. Erros reais de filesystem, conteúdo e protocolo continuam fatais.

Compatibilidade: `PROTOCOL_VERSION` **16 → 17**, documentado no README. PC e APK precisam ser compilados da mesma revisão; o handshake existente rejeita versões diferentes. `model::VERSION` continua 1. Nenhum código Kotlin/JNI foi alterado; a conversão JNI para `ScanDeferred`, a guarda de geração SAF e os wakes continuam ativos. A leitura de `discardScanJson()` confirmou cancelamento/join e restauração de flags, mas isso não substitui teste em aparelho.

| Verificação do snapshot final | Resultado |
|---|---|
| `cargo check --workspace --all-features --locked --offline -j 1` | PASS |
| `cargo test --workspace --lib --bins --locked --offline` | PASS: 143 unitários; três benchmarks existentes ignorados |
| `cargo test --workspace --test scan_deferred -j 1 --locked --offline -- --test-threads=1` | PASS: 17, zero ignores |
| Integrações restantes: `daemon`, `tls_sync`, `v2`, `hash_stream_codec`, `scan_stream` | PASS: 26 |
| `:app:testReleaseUnitTest --rerun --offline --no-daemon --console=plain` | PASS: 65 JVM, 15 classes, zero falhas/erros/skips; JNI de host recompilada |
| `cargo fmt --all -- --check` e `git diff --check` | PASS |

Total: **251 testes permanentes aprovados**, sem contar duas vezes a suíte direcionada nem os benchmarks temporários. Os quatro contratos pré-fix FAIL passaram pós-fix e seus ignores foram retirados. A primeira execução Gradle reaproveitou o resultado de Test; a execução final usou `--rerun` para efetivamente testar contra a biblioteca JNI reconstruída. Sockets locais/cache do host exigiram as aprovações do ambiente.

Cobertura adicional: todas as posições 1–4 de Get; arquivo serial de 9 MiB; oito arquivos com adiamento no segundo lote; namespace legado sem stream ativo; Get após `HashStreamEnd`; uploads PC-only em ToAndroid/bidirecional; wake de outra share cruzando o adiamento; próxima share pelo dispatcher gerenciado na mesma conexão; share/scan/sequence inválidos; I/O real negado; EOF antes da barreira; Put/StagePut/Blob inesperado após adiamento; Blob inesperado durante drenagem e na resposta à barreira. Os testes verificam conteúdo/BLAKE3, base em memória/disco, ausência de `state.next`, staging não publicado, retomada e no-op.

Não foram exercitados aparelho/SAF físico, Wi-Fi, encerramento real do serviço Android nem crash em cada ponto de ACK/Done. O fixture gerenciado testa framing/seleção com IDs distintos sobre um Store artificial; não modela dois bindings SAF. O custo da auditoria integral observado nos traces não foi modificado sem uma causa demonstrada.

Logs finais: `/tmp/rowd-testing-post-fix-check.log`, `/tmp/rowd-testing-post-fix-unit.log`, `/tmp/rowd-testing-post-fix-targeted.log`, `/tmp/rowd-testing-post-fix-integration.log`, `/tmp/rowd-testing-post-fix-kotlin.log`. Resultados XML JVM: `android/app/build/test-results/testReleaseUnitTest/`. Esses artefatos temporários são locais, não fazem parte do commit.

**Comparação em release**

Referência isolada: fontes rastreadas do HEAD pré-fix, extraídas para `/tmp`, e depois o mesmo checkout temporário com `sync.rs`/`protocol.rs` finais. Harness idêntico, SHA256 `5724b9bb1747b99a671ccdfed3e891908a050408bd6753298bfdca1b1ebb72f9`; `cfg!(debug_assertions)` deve ser falso. Ambiente: Rust 1.98.1, Linux/x86_64, filesystem ext4 local em armazenamento MMC, sockets Unix, ToPc. Três repetições por cenário em diretórios novos; cada transferência é seguida de no-op na mesma conexão. São 24 amostras, 12 antes e 12 depois, com assertions de conteúdo/hash e métricas.

| Cenário | Antes: mediana [mín–máx], ms | Depois: mediana [mín–máx], ms | Delta da mediana |
|---|---|---|---|
| 16 arquivos de 4 KiB | 260,467 [254,364–274,274] | 277,000 [258,183–322,015] | +6,35% |
| Arquivo serial de 9 MiB | 147,273 [124,655–166,927] | 136,301 [130,949–155,773] | −7,45% |
| No-op dos arquivos pequenos | 21,900 [19,911–23,170] | 21,972 [21,715–24,405] | +0,33% |
| No-op do arquivo grande | 34,124 [31,888–47,537] | 35,749 [35,336–36,629] | +4,76% |

Resultado semântico: **PASS**, com bytes transferidos/hasheados, quantidade de hashes, controles, ACKs, persistências e staging idênticos nas três repetições antes/depois. A janela de arquivos pequenos continua quatro/16 KiB neste dataset; arquivo grande permanece serial; no-op continua zero transferências. Resultado quanto a mudança de tempo: **INCONCLUSIVO**, pois há apenas três amostras e todas as faixas se sobrepõem. Não se atribui ganho ou perda causal a esses deltas, nem se extrapola para Wi-Fi/SAF ou datasets grandes. O fixture LocalStore rehasha no no-op, inclusive os 9 MiB; não representa correção do custo de auditoria original.

As medições autoritativas executaram diretamente os binários release salvos, depois de terminar os builds/testes funcionais, sem compilação concorrente. Warmups e tempo de compilação foram excluídos. Uma tentativa de usar o binário antigo por timestamps preservados na cópia foi descartada; o rebuild correto consta em `fixed-build-warmup-release.log`. Os executáveis finais têm hashes distintos: baseline `02dd4830b4433460494bf906492365e63c0eae1120458e1589fe69b92b15337c`, corrigido `93fd8f99a37411ed2a839ad3eaf60ec35d1e30f5d893fb873daa6aff6b934864`. Root conferiu independentemente as 24 linhas brutas, medianas, assertions de métricas e os hashes das fontes copiadas contra o snapshot validado.

Artefatos: `/tmp/rowd-deferred-perf-20261011/{baseline-release.log,fixed-release.log,samples.json,summary.json,environment.json}` e o harness temporário em `source/crates/rowd-core/tests/perf_deferred_window.rs`. A comparação não adiciona um benchmark permanente ao produto.

Próxima ação principal: **PROMOTE_REGRESSION**. Concluída neste working tree: os contratos reproduzíveis estão ativos na suíte normal, com regressões adversariais para a mesma classe causal. Isso protege a correção contra reincidência. A implantação V17 e a validação em aparelho permanecem fora desta campanha de testes locais.
