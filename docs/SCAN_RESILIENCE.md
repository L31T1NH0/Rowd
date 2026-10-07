# Scan, cancelamento e resiliência

Correção sobre o workspace existente, preservando mudanças locais anteriores.
Rust síncrono, Kotlin e SAF mantidos; nenhuma dependência nova e nenhum Tokio.

## Bloco 6: testes físicos e entrega 0.8.4-alpha

O bloco 6 corresponde aos testes físicos, que serão executados pelo usuário.
Os testes automatizados dos blocos anteriores não substituem validação com
celular, provedor SAF e rede reais. O cenário de 2.000 arquivos com média de
2 MB, interrupção/retomada, edições concorrentes e consumo de recursos ainda
precisa dessa execução física; não se declara esse bloco validado em dispositivo.

A versão do workspace Rust e do Android é 0.8.4-alpha, versionCode 17.
O protocolo permanece V15. Desktop e APK devem vir deste mesmo checkout.

Entrega compilada em 07/10/2026: build release de todo o workspace Rust,
biblioteca aarch64-linux-android e assembleRelease do Android concluídos.
Kotlin/Java e lintVitalRelease passaram no build completo. A assinatura do
APK foi verificada com apksigner; aapt confirmou app.rowd, versionCode 17,
versionName 0.8.4-alpha, minSdk 26 e ABI arm64-v8a. O comando rowd aponta
para target/release/rowd por ~/.local/bin/rowd e retorna rowd 0.8.4-alpha.
Os testes físicos continuam pendentes; a falha anterior do teste de FIFO
registrada no bloco 5 permanece conhecida.

## Bloco 5: instalação no PC sem cópia duplicada

- O receptor pede ao Store um diretório de staging. LocalStore recebe em
  `.rowd`, calcula SHA-256 enquanto grava e consome o temporário verificado
  na instalação. A publicação usa persist_noclobber: não cria outra cópia
  do payload nem sobrescreve um arquivo recriado por um editor.
- O caminho atende GETs em janela, arquivos acima de 8 MiB, recebimento PUT
  e InstallStaged. O adaptador `&mut Store` encaminha também os métodos novos.
  Snapshots emprestados para encaminhar conflitos continuam disponíveis;
  esses casos e temporários externos usam cópia com hash durante a cópia.
  Plataformas sem fingerprint confiável conservam esse fallback.
- VerifiedStaged registra identidade, tamanho, mtime e ctime. As guardas
  conferem o temporário antes da instalação e novamente antes da publicação.
  Alteração após a verificação não publica bytes tratados como verificados.
- A checagem final do namespace permanece. Arquivos instalados com fingerprint
  Unix intacto dispensam outra leitura do conteúdo. Fingerprint diferente ou
  desconhecido provoca hash e conferência de estabilidade; conteúdo alterado
  continua sendo STALE_TARGET, inclusive com tamanho e mtime restaurados.
- Fsync, journal e backup do inode deslocado continuam no caminho comum.
  Repetição após ACK perdido continua idempotente e o alvo é conferido antes
  e depois de calcular seu hash. Invalidação durável do cache continua ativa.
  A otimização não evita por si só leituras de uma próxima varredura.

Para 2.000 arquivos novos com média de 2 MB, a publicação direta elimina
aproximadamente 4 GB de leitura e 4 GB de gravação da segunda cópia no PC.
Também evita aproximadamente 4 GB de leitura em cada validação final quando
os arquivos permanecem intactos. São volumes lógicos derivados do código,
excluindo journal, cache, observadores e custos do filesystem. A recepção ainda
grava os 4 GB e verifica SHA-256; continuam existindo operações de metadados e
fsync por arquivo. Não foi executado benchmark real de 4 GB nem medido tempo
em celular/PC. Substituições e conflitos mantêm verificações e backups próprios.

Validação: 76 testes unitários de rowd-core e 12 de rowd-android passaram
com três benchmarks ignorados e a pendência conhecida de FIFO filtrada.
Os sete testes de scan_stream e dois de hash_stream_codec passaram. O cenário
de cinco arquivos de 2 MiB e um de 9 MiB verifica conteúdo, seis transferências,
zero staging_copies e zero hashes extras do Store no PC; o hash da recepção
continua sendo calculado. Os testes de 1.040 arquivos e interrupção no arquivo
500 usam publicação direta, verificam ausência de cópia adicional e retomada
dos 540 restantes. Testes unitários cobrem inode, backup, fallback externo,
temporário alterado e edição posterior com mtime restaurado.

Cargo check do workspace com todas as features passou. A pendência isolada
`sync::v2_tests::conflict_drains_fifo_window` foi executada novamente e falha
com peak_in_flight = 2, esperado 1, como antes deste bloco. O protocolo continua
V15, sem alteração de mensagens. Não houve build do APK neste bloco.

## Bloco 4: descoberta e consultas SAF em lote

- Namespace completo/profundo e descoberta incremental consultam os seis
  campos de metadados em uma projeção por diretório. Uma consulta adicional
  valida a identidade/tipo do próprio diretório, incluindo diretórios vazios.
  A enumeração não chama name/isFile/isDirectory/length separadamente por filho.
- URIs desconhecidas acumuladas solicitam uma única descoberta de metadados
  a partir da raiz selecionada. Saiu o loop URI × diretório e o corte de 128
  diretórios. Descoberta continua recursiva: timestamp do diretório não prova
  que o conteúdo de seus descendentes permaneceu igual.
- Prefixos ancestrais absorvem hints descendentes; índices de diretórios de
  subárvores visitadas são reconstruídos, removendo entradas desaparecidas.
  A enumeração acontece fora de scanLock. Callbacks podem adicionar novos
  hints enquanto o worker consulta o provedor; publicação e fallback usam lock.
- ScanPathLookup guarda listagens somente na operação atual. Caminhos irmãos
  compartilham listagem e resolução de ancestrais; a revalidação busca nomes
  selecionados, ausências anteriores e associações de ancestrais novamente.
  Troca de URI com tamanho/data iguais continua sendo STALE_SOURCE. O mesmo
  agrupamento atende o caminho legado e o fallback de ancestry no hash stream.
- Metadados de um documento exato também usam uma projeção única. A guarda
  antes/depois do hash e a checagem final de namespace permanecem ativas.
  Listagem incompleta (EXTRA_LOADING), cursor ausente, IDs/nomes duplicados,
  nomes inválidos e tipos virtuais provocam fallback/erro, sem manifest parcial.
- O observador de cursor informa o URI do diretório que mudou. Quando esse
  diretório é conhecido, a descoberta fica restrita à sua subárvore. Notificações
  de diretórios são agrupadas na janela fixa de 100 ms já existente.

No teste do índice, 2.000 arquivos irmãos exigem uma listagem inicial e uma
revalidação. No adaptador SAF, cada listagem equivale a duas consultas (documento
do diretório + filhos). A leitura do conteúdo e as verificações de fonte continuam
por arquivo. O limite de 1.024 caminhos do incremental não mudou: acima dele,
o fallback completo usa a enumeração em lote. Não se mediu ganho de tempo em
um dispositivo real.

Validação: 39 testes JVM passaram, incluindo seis do índice e cinco do diff
de metadados. Cobrem 2.000 irmãos, substituição com size/modified iguais,
ancestral movido/recriado, caminho antes ausente, cancelamento e separação de
prefixos. Fontes Android alteradas e testes instrumentados de metadados e
observadores compilaram no compilador Kotlin local em modo 2.0. Os testes
instrumentados foram apenas compilados: precisam de dispositivo/emulador para
executar. Não houve build completo do APK. Stamps e git diff --check passaram.
Protocolo permanece V15; a pendência de FIFO anterior não foi alterada.

## Bloco 3: observação e agendamento por Share

- Geração de mudança é consultada por Share e permanece estável após consumir
  sua pendência. Um wake específico de B não invalida o scan/cache físico de A;
  wake manual global continua invalidando todos. Reconexão continua separada.
- O Android nativo devolve completed_shares com os Shares que receberam Done.
  O serviço confirma somente esses IDs e somente a versão capturada no início
  da rodada. Auditoria de A não apaga pendências de B nem eventos mais recentes.
- Pendências que sobraram de uma auditoria ou rodada adiada geram a próxima
  passagem imediatamente. Uma rodada normal sem Share atendido não entra em
  retry imediato, evitando loop com Shares indisponíveis.
- O relógio da auditoria profunda pertence a cada Share. A profunda de A não
  reinicia o intervalo de B; o relógio só avança quando o Share foi concluído.
- Discard/cancelamento restaura também solicitações agendadas de varredura
  completa/profunda, tanto no stream quanto no caminho legado. Não se converte
  silenciosamente uma profunda cancelada em varredura com reuso de hashes.
- A decisão entre auditoria periódica e solicitação global usa a geração
  capturada sob lock, evitando uma segunda leitura concorrente fora do lock.

A auditoria normal continua percorrendo um Share por vez, com o intervalo
existente de um minuto. A profunda é selecionada quando o Share volta ao cursor
e seu próprio intervalo de 15 minutos venceu. Callbacks genéricos do provedor
sem identificação do Share ainda podem indicar mudanças em múltiplos Shares.
Não mudou o protocolo V15: completed_shares é retorno interno Rust → Kotlin.

Validação: 28 testes JVM (incluindo quatro novas regressões de WakeState) e
12 testes rowd-android passaram; FolderAccess e SyncService com seus helpers
compilaram no compilador Kotlin local em modo 2.0. Formatação Rust, stamps e
git diff --check passaram. Não houve build completo de APK nem execução em
dispositivo; os testes não simulam callbacks reais de um provedor SAF.
O teste de FIFO pendente, registrado no bloco 2, não foi alterado neste bloco.

## Bloco 2: continuidade da varredura incremental (V15)

A descoberta de metadados e o hashing focado agora executam no ScanWorker.
Enquanto o worker trabalha, o Android envia ScanAlive a cada 5 segundos e
aceita AuditPreempt. O coordenador usa o mesmo gate de inatividade da completa:
ScanReady → ScanContinue → DeltaManifest ou NeedFullScan. ScanDeferred encerra
a rodada sem promover sua base. Cancelamento durante descoberta solicita uma
nova enumeração, pois os hints já podem ter sido consumidos. O worker termina
antes da seleção da próxima Share. Uma chamada SAF bloqueada pelo provedor
continua sem poder ser interrompida entre os pontos cooperativos de controle.

O protocolo passou de V14 para V15 porque a resposta incremental ganhou esse
gate; PC e APK precisam ser compilados e atualizados juntos.

O cache físico só grava quando há alterações: checkpoints têm intervalo mínimo
de 15 segundos, incluindo tentativas que falharam, e o encerramento força uma
última tentativa. Falha de persistência registra HASH_CACHE_SAVE_FAILED,
preserva os hashes em memória e permite retry; não aborta uma leitura válida
nem substitui a exceção original durante limpeza. A gravação mantém temporário,
fsync e substituição atômica. A completa e a profunda usam esse mesmo helper.

Regressões específicas: gate incremental com 360 segundos simulados; resposta
e gate seguinte alinhados; responder incremental com sucesso, fallback,
cancelamento durante trabalho e preempção depois de ScanReady; checkpoints
sem alterações, intervalo de gravação, falha de disco e recuperação do cache.

Validação deste bloco: 71 testes rowd-core e 12 rowd-android passaram (3
benchmarks ignorados); 18 testes JVM passaram e FolderAccess com os helpers
alterados compilou no compilador Kotlin local em modo 2.0. A regressão de
cancelamento também exercita três rodadas na mesma conexão e verifica que a
rodada adiada não altera a base em memória nem em disco. Formatação Rust,
stamps de trace e git diff --check passaram.

O teste preexistente conflict_drains_fifo_window foi executado separadamente
e continua falhando: espera pico de 1 arquivo em voo e observa 2. Foi excluído
da execução citada de 71 testes; não se alterou essa asserção neste bloco.
Não houve build completo de APK nem teste em dispositivo. As validações mais
abaixo descrevem trabalhos anteriores e não substituem estes resultados.

## Cadeias causais e correções

1. **Deadline absoluto:** `session_round` → `coordinate_candidate` → `Scan` →
   `receive_scan_gate` → `Waiting::read`. WouldBlock/TimedOut era reemitido depois
   de 90 s desde o início; a sessão enviava Error e invalidava a conexão.
   Android `scan_with_control` agora envia ScanAlive a cada 5 s enquanto o worker
   está ativo. Waiting renova 90 s de inatividade somente ao consumir liveness
   completo. Bytes parciais não renovam a espera. Um peer silencioso ainda expira.
   O relógio injetável permite testar seis minutos sem dormir seis minutos.

2. **Cancelamento condicionado:** `deferScanJson` marca scanAbort; `walk` e o
   staging final só consultavam esse flag dentro de auditScan. Agora cancelamento
   explícito sempre interrompe. Mudança de geração permanece condicionada à
   semântica da auditoria. Share/binding alterado também interrompe o scan.
   O cleanup Rust mantém defer → finish do worker → discard antes de trocar Share.

3. **Digest sem cancelamento:** o loop de SHA só consultava deadline local.
   `scanDigest` fecha InputStream com use e consulta controle antes/depois de
   cada leitura de 64 KiB, inclusive antes de devolver o hash final. Scans passam
   um controle explícito; snapshot/recovery preservam o controle anterior de
   deadline. Hash interrompido não entra no cache. Metadados pré/pós hash são
   comparados para rejeitar arquivo que mudou durante a leitura.
   A proteção SAF de 30 minutos passou a ser por hash/enumeração; o scan saudável
   inteiro não possui mais esse teto absoluto.

4. **Hash dependente de commit:** nextCache só era publicado em commitScanJson;
   discardScanJson perdia hashes válidos junto com o staging protocolar.
   PhysicalHashCache recebe hashes completos independentemente de pendingScan,
   scanReady, base token, manifest e ACK. Ele não fornece listas de arquivos nem
   decide delta: a enumeração atual continua obrigatória para um full scan.
   scanCache/índices/scanReady continuam sendo publicados apenas em commit.

5. **Erros assimétricos e sem contexto:** o responder atual já enviava Error no
   epílogo, mas usava to_string, sem operação/path/classe, tentava reenviar erros
   recebidos e podia tentar enviar depois de transporte morto. ShareError mantém
   causa e classificação tipadas através de anyhow. O responder registra a
   operação/path e envia nos pontos válidos; PC envia com contexto de Share.
   Abertura do staging ocorre antes de anunciar Put/Blob. Falha em frame/Blob
   incompleto impede inserir outro frame. PeerError/ShareError não são refletidos.
   Erro de instalação é enviado depois de o lote ter sido completamente recebido.
   Classificação de causa Android e necessidade de reconectar são decisões
   independentes: filesystem + stream_reusable=false ainda descarta o stream.

6. **Polling ruidoso:** interrupted_with_control emitia IO_SYSCALL_FAILED para
   qualquer erro, antes de os callers reconhecerem idle. Callers que esperam
   WouldBlock/TimedOut usam poll/poll_with_control, que suprimem só esses eventos
   individuais. Erros reais e retry de EINTR continuam registrados. I/O normal
   não recebe essa supressão. Não se acrescentou polling nem se alterou discovery.

## Protocolo

Versão de protocolo 12 → 13, com autenticação rejeitando versões incompatíveis
antes de enviar os novos frames. PC e Android precisam ser atualizados juntos;
a versão de release do workspace não foi alterada.

Sequência normal: Scan → zero ou mais ScanAlive scoped → ScanReady → ScanContinue
→ manifest → transferências/ACK → Done. O mesmo thread envia liveness e ScanReady;
ele para de enviar liveness quando consome o resultado do worker. Não existe um
heartbeat paralelo que possa continuar enviando depois de ScanReady.

AuditPreempt continua válido durante a espera e na janela de ScanReady. Frames
anteriores de liveness são consumidos na gate; ScanDeferred conclui a preempção.
Nenhum liveness é aceito como mensagem normal durante manifest/transferência.

ShareError é um frame de controle com side (pc/android ou responder nos testes
host), Share ID opcional, operação, path opcional, classe, cadeia de mensagens e
stream_reusable. Falhas fatais de Share anunciam false: não há handshake de
recuperação de uma rodada incompleta. ScanDeferred mantém sua recuperação e
reutilização existentes. Erros de seleção antes da rodada usam contexto próprio.

## Cache físico persistente

Um arquivo privado por Share, em filesDir/physical-hashes. Nome derivado do ID;
payload inclui schema 1, Share ID, bindingIdentity (tree, revisão e ignore), path,
URI, lastModified, length, SHA-256 e tamanho lido. O arquivo contém checksum
SHA-256 do payload, limites de tamanho/quantidade e validação completa ao carregar.
Schema desconhecido, corrupção, truncamento ou binding diferente descartam o
cache carregado e causam novo hashing.

Só há reuse com modified positivo, URI/path/binding iguais e length == size.
Deep audit não reutiliza hashes. Arquivos que deixam de existir são podados
após enumeração completa. Não se guarda hash parcial.

Persistência: temporário no mesmo diretório → flush → fsync → rename atômico;
falha de persistência preserva o cache em memória e registra HASH_CACHE_SAVE_FAILED.
Os hashes completos também são salvos quando o scan termina por cancelamento,
sem publicar um manifest incompleto. Commit/discard não controlam esse arquivo.

Dirty restaurado para retry não deve apagar conhecimento físico. Cada hash guarda
em memória a geração de mudanças observada antes de seu cálculo. Um path dirty
pode reutilizá-lo somente nessa mesma geração. Evento posterior invalida esse
reuse mesmo com metadados iguais. Gerações não são persistidas: após restart,
qualquer path novamente dirty exige hash; metadados de arquivos sem novo dirty
podem reutilizar o cache persistido.

## Comportamentos adicionais investigados

- receive_put_batch dava prioridade ao erro de instalação mesmo quando o reader
  falhava depois. Isso mascarava transporte/recepção incompleta. Agora verifica o
  reader antes de reportar uma falha de instalação; a falha local já foi registrada
  no ponto de instalação. Com lote completo, a causa local é enviada ao peer.
- commitScanJson validava Share/tree, mas não toda a identidade do binding. Agora
  verifica também revisão/ignore capturados pelo scan.
- O novo contexto tipado inicialmente ocultou a mensagem original em to_string.
  Um teste existente de colisão detectou isso. LocalOperation agora conserva a
  mensagem, sem enfraquecer esse teste.
- Uma reconciliação completa reafirma via ACK arquivos iguais (Action::None).
  Essa confirmação em rodadas diferentes é comportamento existente, não nova
  transferência. O teste de reconexão conta snapshots/Blobs separadamente de ACK.

## Queda anterior durante transferência

O deadline de receive_scan_gate só atua antes de manifest e não explica o EOF
observado depois de várias instalações. Não foram encontrados traces brutos
adicionais no workspace que permitam atribuir essa queda a uma causa específica.
Não se alteraram callbacks de rede, assinatura de interface ou geração de rede.

Caminhos locais de snapshot/install agora produzem ShareError e trace com operação,
path e causa. EOF real e falhas de Blob/transporte incompleto continuam sendo
transporte; não é possível inserir um Error dentro dos bytes de um Blob anunciado.
O trace preserva a causa local quando esse encerramento for necessário.

## Cobertura de regressões

| Caso solicitado | Verificação |
| --- | --- |
| 1. Scan >90 s | Gate com 360 s de relógio falso, liveness periódico, manifest e próxima gate alinhados; teste real de rodada com ScanAlive |
| 2. Peer morto | Timeout 90 s após último liveness, classificado TimedOut |
| 3. Full scan não-audit cancelado | Matriz de cancelamento independente de audit, ScanWorker cooperativo termina/volta idle/aceita próxima Share |
| 4. SHA cancelado | Reader controlado interrompido no terceiro chunk, stream fechado, sem hash retornado; cache invalidado não persiste parcial |
| 5. AuditPreempt | Liveness anterior drenado, ScanDeferred recebido, nenhuma ScanContinue extra; semântica de geração de auditoria testada |
| 6. Erro local responder | Snapshot e install injetados chegam ao coordenador como ShareError com filesystem, operação/path e causa |
| 7. Erro local PC | Serialização de erro local com contexto de snapshot; classificação Android mantém filesystem e invalida stream quando indicado |
| 8. Disconnect | EOF/BrokenPipe/ConnectionReset não geram erro lógico; Blob incompleto não recebe frame Error; testes existentes de TLS/socket |
| 9. Reuse após falha | 1033 hashes físicos sem commit, salvos/recriados; dirty de retry na mesma geração também reutiliza |
| 10. Invalidação | Size, modified, URI, path, Share, tree/revisão, metadata desconhecido, dirty de nova geração e cache malformado |
| 11. Restart | Nova instância de PhysicalHashCache carrega os 1033 hashes; schema/checksum/truncamento inválidos causam fallback |
| 12. Trace polling | 10000 WouldBlock/TimedOut sem IO_SYSCALL_FAILED; syscall normal falha e preserva errno/contexto; EINTR mantém retry |
| 13. Confirmados não repetem | Nove transferências, rodada seguinte zero, reconnect zero, total de snapshots permanece nove; lost-ACK e FIFO recovery existentes |
| 14. Persistência da conexão | Duas rodadas reais na mesma conexão com 25 ScanAlive por scan e nenhum desalinhamento |

Os testes de Kotlin são JVM e exercitam os mesmos helpers de digest/cancelamento/cache
usados em FolderAccess; não substituem uma execução em dispositivo com provedor SAF.
O teste de tempo longo usa relógio falso; os testes de integração usam sockets reais.

## Limites

- Um provedor que muda conteúdo preservando size e modified, sem notificar mudanças,
  não permite detectar isso por metadados. Deep audits permanecem como proteção.
- Cancelamento entre chunks não pode interromper uma única chamada SAF bloqueada
  dentro do provedor; não se fecha um stream de outro thread de forma insegura.
- Cache é checkpointado ao terminar/abortar scan. Kill do processo durante hashing
  pode perder hashes ainda não checkpointados, preservando o arquivo anterior.
- O timeout de transporte das demais fases não foi redesenhado. A causa do EOF
  específico relatado durante transferência permanece não demonstrada.
- A simulação automatizada não é uma reprodução física dos 1033 screenshots no
  Android original. Não houve instalação/deploy neste trabalho.

## Arquivos deste trabalho

| Arquivo | Alteração |
| --- | --- |
| android/app/src/main/java/app/rowd/FolderAccess.kt | Controle independente, digest cooperativo, cache físico e binding no commit |
| android/app/src/main/java/app/rowd/ScanDigest.kt | SHA em chunks com controle opcional e fechamento garantido |
| android/app/src/main/java/app/rowd/PhysicalHashCache.kt | Evidência física persistente, validação, checksum e escrita atômica |
| android/app/src/test/java/app/rowd/ScanResilienceTest.kt | Nove regressões JVM |
| crates/rowd-core/src/protocol.rs | Protocolo 13, ScanAlive, ShareError tipado, proteção de frames incompletos e dois testes |
| crates/rowd-core/src/sync.rs | Gate de inatividade, erros contextualizados, prioridade do reader e seis regressões |
| crates/rowd-core/src/io_retry.rs | API explícita de polling sem falhas esperadas no trace |
| crates/rowd-core/src/trace.rs | Regressão de 10000 polls e verificação de falhas reais |
| crates/rowd-core/src/discovery.rs | Uso da API de polling na espera UDP |
| crates/rowd-core/src/pairing.rs | Uso da API de polling na espera UDP |
| crates/rowd-core/src/managed.rs | Erros estruturados de seleção de Share |
| crates/rowd-android/src/lib.rs | Emissão de liveness, polling e invalidação independente da classe da causa |
| crates/rowd-android/src/transport.rs | Classificação de ShareError, requires_reconnect e regressão |
| crates/rowd-app/src/lib.rs | Polling esperado e envio estruturado de erro local sem reflexão/duplicação |
| docs/TRACE_SCHEMA.md | Semântica de polling, liveness e erros |
| docs/SCAN_RESILIENCE.md | Cadeias, correções, cobertura, validação e limites |

Arquivos/modificações preexistentes (incluindo ScanWorker, seus testes, watcher,
Cargo.toml, análise de reconexão e exclusões de documentação) foram preservados.

## Validação

Rust: `cargo test --workspace --no-fail-fast` final: **138 passaram, 0 falharam,
3 ignorados**. Os três ignorados são benchmarks de escala explicitamente marcados
com ignore no código existente, não testes funcionais desabilitados por esta correção.
Unitários, integração TCP/TLS/UDP/Unix, CLI/daemon e doc-tests foram executados.

Os testes de utilitários de trace Python também passaram: 4 de validação de idle
mais 5 de alinhamento de relógios. `git diff --check` e stamps Kotlin passaram.

Falhas intermediárias: o sandbox impediu a criação de sockets (Operation not
permitted) e a inicialização nativa do Gradle. As suítes foram repetidas com
permissão apropriada. Um teste existente detectou causa ocultada pelo novo contexto,
corrigida sem mudar a asserção. O teste novo de confirmação esperava nove ACKs
quando a reconciliação completa corretamente reafirmava esses arquivos na segunda
rodada; o teste passou a distinguir ACKs de snapshots/transferências. Erros de
compilação durante a implementação foram corrigidos antes da execução Rust final.
A execução intermediária de Kotlin também misturou uma classe compilada antes da
última edição com o teste novo da geração dirty; foi repetida sobre fontes estáveis.

Kotlin final: `gradle -p android :app:testReleaseUnitTest :app:testDiagnosticUnitTest
--offline --console=plain`, **30 passaram em release e 30 em diagnostic, zero
falhas/erros/skips**. Gradle concluiu BUILD SUCCESSFUL; ambas as variantes compilaram
FolderAccess e os helpers finais. XMLs em android/app/build/test-results.

| Suíte Rust final | Passaram | Falharam | Ignorados |
| --- | ---: | ---: | ---: |
| rowd CLI unitários | 10 | 0 | 0 |
| integração daemon | 1 | 0 | 0 |
| integração TLS/resolver | 7 | 0 | 0 |
| integração v2 | 9 | 0 | 0 |
| rowd-android | 12 | 0 | 0 |
| rowd-app | 25 | 0 | 0 |
| rowd-core | 70 | 0 | 3 |
| rowd-daemon | 4 | 0 | 0 |
| doc-tests | 0 | 0 | 0 |
| Total | 138 | 0 | 3 |

Foram adicionados nove testes Rust e nove Kotlin, além de fortalecer a verificação
existente de falha de FIFO e a de tracing. Os logs finais desta sessão estão em
/tmp/rowd-rust-final.log e /tmp/rowd-kotlin-final.log. Não existem falhas pendentes
nas suítes funcionais executadas.
