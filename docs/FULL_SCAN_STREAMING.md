# Full scan em fluxo — protocolo 14

## Arquitetura implementada

Somente full scans usam o novo caminho. `DeltaScan` mantém a negociação,
manifest incremental, dirty paths e reconcile existentes.

```
recuperação existente
→ enumeração SAF sem SHA / namespace PC
→ namespace completo e validação local + cruzada
→ hashes PC necessários para reconcile
→ worker SAF: hash / reuse / staging de source
→ HashChunk → reconcile por path → transferência
→ HashStreamEnd + commit do cache do scan
→ instalação dos recebimentos PC → Android que estavam staged
→ demais trabalhos, conflicts e ACK
→ pending / state.next / Done / rename / remove pending
```

O namespace contém paths, tipo, length e lastModified; o Android mantém as URIs
privadamente. Paths inválidos, nomes reservados, colisões de case inclusive em
diretórios implícitos, file/directory, duplicatas do provider e limites são
rejeitados antes de iniciar o hash stream e qualquer transferência. `MAX_FILES`
continua 100.000; o namespace também limita diretórios a 200.000 entries totais.
As rotinas de recovery existentes rodam antes de construir esse snapshot.

O PC conhece a ausência estrutural após fechar o namespace. Paths presentes no
Android só entram no reconcile quando seu SHA chega. O reconcile e as restrições
de direção permanecem os existentes. Conflicts são resolvidos após o hash stream,
preservando a cópia Android em ambos os lados antes de substituir o original.

O PC ainda calcula os hashes locais necessários antes de começar o worker SAF.
Isso evita introduzir outro scanner concorrente nesta implementação. A
sobreposição principal é hashing SAF × transferência, especialmente Android → PC.

## Frames e versão

`PROTOCOL_VERSION`: **13 → 14**. Hello rejeita versões diferentes antes da
negociação de scan; PC e Android precisam ser atualizados juntos.

| Frame | Significado |
| --- | --- |
| ScanStreamBegin | Nova identidade aleatória de scan e binding |
| NamespaceChunk | Entries estruturais, sequência monotônica |
| NamespaceEnd | Fecha quantidade, sequência e digest SHA-256 do namespace serializado |
| HashStageHint | Paths elegíveis para staging junto ao hash, após validação global |
| HashStreamBegin | Autoriza hashing do namespace validado e confirma binding |
| HashNext | Solicita a próxima sequência |
| HashChunk | Até 32 Entries com SHA e size |
| HashRelease | Libera sources privados do lote já processado |
| HashStreamEnd | Fecha cobertura integral, sequência e métricas |
| StagePut + bytes | Recebimento PC → Android em staging privado verificado |
| StageReceived | Confirma apenas receipt privado; não é ACK de instalação |
| InstallStaged | Instala um receipt específico depois do commit do scan |

`last_sequence` é a próxima sequência esperada: zero para stream vazio.
Hashes devem cobrir exatamente os arquivos do namespace, com sizes iguais.
IDs antigos, duplicatas e saltos são rejeitados pelos estados de namespace,
hash, receipt e instalação. Nova conexão cria novo estado de responder e novo
scan ID; não existe protocolo de resume de chunks.

`ManifestChunk` e `MANIFEST_CHUNK_FILES = 1024` continuam nos usos anteriores,
inclusive ACK. O novo `SCAN_STREAM_CHUNK_FILES = 32` reduz a espera pelo primeiro
lote e o custo de JSON/JNI por arquivo. Um lote pode ser menor quando o pool de
staging exige liberar sources antes de produzir mais arquivos.

## Threading, limites e backpressure

Rust mantém protocolo/socket na thread da sessão, sem Tokio. `ScanWorker` enumera
o namespace; depois `ScanStreamWorker` calcula hashes numa thread Kotlin. Não há
duas threads escrevendo simultaneamente no socket.

O worker publica numa `ArrayBlockingQueue` de **dois chunks**. Quando cheia, ele
aguarda em intervalos de 50 ms que também verificam cancelamento. Não descarta
chunks. O namespace fechado permanece em memória porque a validação global
precisa dele; a produção de resultados de hash é limitada separadamente.

Sources staged durante hash têm pool de **quatro arquivos / 8 MiB**. Se lotar,
o worker publica o lote parcial e espera liberação. Receipts PC → Android usam
outro pool limitado a **quatro arquivos / 8 MiB**, fora do SAF. Quando esse pool
fica cheio, os demais ToAndroid aguardam o fim do stream; não se bloqueia o hash
esperando instalações proibidas durante o scan. Os dois pools têm, portanto,
orçamento combinado de até 16 MiB de conteúdo staged em disco nessa fase.
Snapshots já entregues ao caminho de transferência mantêm os limites anteriores;
16 MiB descreve os dois pools novos, não todo o staging da sessão. Os bytes staged
são arquivos privados em disco; a fila de chunks contém apenas metadata/Entries.

Arquivos acima de 8 MiB continuam pelo caminho serial depois dos limites
aplicáveis; `MAX_FILE = 8 GiB` não mudou. Não entram nos pools pequenos.

## SAF, sources, cache e writes

O worker usa path → URI enumerada. Confere tree, binding, ancestralidade, nomes,
tipo file, ausência de virtualidade e metadata antes/depois do hash. Quando o
provider não suporta ancestralidade, o fallback exige encontrar exatamente a
mesma URI; nunca aceita outro arquivo apenas por ter o mesmo nome. Esse fallback
é medido. O caminho normal não usa `find(path)` por arquivo.

Quando um miss do PhysicalHashCache pode ser enviado ao PC, `scanDigest(input,
stagingOutput)` produz SHA e source privado numa leitura SAF. O snapshot reutiliza
esse arquivo, revalida source/binding e Rust continua exigindo `VerifiedStaged`.
Hits do cache não obrigam staging: só abrem o source se reconcile pedir envio.

PhysicalHashCache é evidência física, não existência nem base confirmada. O
namespace sempre é enumerado novamente. Hashes completos são persistidos a cada
32 arquivos e no encerramento, inclusive cancelamento. Reuso exige URI, path,
binding e metadata adequados; modified não confiável não autoriza reuse. Novos
full scans removem sources privados órfãos de um processo anterior.

PC → Android recebe bytes sem tocar SAF. `HashStreamEnd` só é enviado depois de
`commitScanJson`; então `InstallStaged` usa os mesmos preconditions e recovery do
install normal. A troca de `scanCache`, `uriPaths` e índices de diretório ocorre
antes desses installs, que atualizam os caches existentes. `install()` também
rejeita qualquer chamada enquanto o namespace SAF estiver ativo.

O PC registra path + Entry esperada e diretórios que criará antes do install.
O watcher consulta esse registro funcional e verifica conteúdo físico antes de
suprimir o evento. Hash diferente, delete ou evento desconhecido continuam sendo
edição externa. O registro tem expiração e espera limitada por install ativo;
I/O muito lento pode provocar preempção conservadora em vez de ocultar uma edição.

## Mutações, preempção, crash e sessão

O worker revalida todas as associações URI/metadata e o conteúdo estrutural de
cada diretório antes de fechar o snapshot. Binding compartilhado entre instâncias
de FolderAccess invalida scans antigos após remap, revisão ou ignore novos. O
commit confere binding/generation antes de substituir qualquer cache.

O PC revalida namespace e fingerprints antes de autorizar hash remoto e antes de
Done, incluindo writes próprios e conteúdo dos arquivos instalados. Mudanças
externas podem solicitar preempção também em full scans de rodadas focadas.
Metadata/provider inconsistente preserva STALE_SOURCE; install continua usando
STALE_TARGET e preservação do conteúdo editado.

Preempção só em frame boundary: Blob atual termina antes de AuditPreempt. O
coordenador para de agendar paths, drena os Blobs em andamento, cancela/junta o
worker e usa ScanDeferred/RoundDeferred. Falha antes de consumir o body de um
StagePut marca o framing incompleto e fecha a conexão.

Nenhum HashChunk ou StageReceived promove base. A state candidata só vira
committed ao final, pelo mecanismo pending/state.next/Done/rename existente.
Queda mantém installs físicos válidos e base anterior; receipts privados não
instalados são descartados. A próxima rodada enumera o estado físico real e pode
reusar hashes. ACK perdido continua usando install idempotente.

O listener mantém candidatos separados, com limite e timeout de handshake.
Somente após TLS, HMAC válido e conferência do dispositivo pareado o candidato
pode substituir a sessão ativa. Conferência de revogação e promoção usam a mesma
trava de configuração. Port scan, TCP silencioso, HMAC inválido e `device test`
não promovem candidato.

## Arquivos centrais

- `crates/rowd-core/src/{model,protocol,storage,sync,managed,internal_writes}.rs`
- `crates/rowd-core/src/lib.rs`
- `crates/rowd-app/src/{lib,watcher}.rs`
- `crates/rowd-android/src/lib.rs`
- `android/app/src/main/java/app/rowd/{FolderAccess,ScanStreamWorker}.kt`

ScanWorker, ScanDigest, PhysicalHashCache e mudanças de resiliência já existentes
no workspace foram reaproveitados. O diff total inclui mudanças anteriores a esta
implementação; deleções de outros documentos não foram feitas para este pipeline.

## Métricas e situação da validação

Há traces de namespace/hash, backpressure, staging/reuse, writes internos e
preempção. O relatório inclui namespace_ms, time_to_first_hash_ms,
time_to_first_transfer_ms, hashes_calculated/reused, bytes_hashed,
files_staged_during_hash, duplicate_reads_avoided, queue_peak_chunks,
queue_wait_ms, contadores de transferências antes do HashStreamEnd e fallback SAF.
`manifest_bytes` contabiliza os payloads JSON dos hashes, sem overhead dos frames.

Antes: aproximadamente 86 s para o scan informado pelo usuário, seguido da
transferência. Depois: não há medição real nova desse Share. Os contadores permitem
comparar no aparelho sem atribuir um ganho ainda não observado.

Nesta etapa final **não foram executados nem adicionados testes**, conforme a
instrução mais recente. `cargo check --workspace --all-features --locked` passou.
Os testes da etapa anterior tinham 144 passes Rust, três ignored, zero falhas e
31 passes Kotlin puro. Isso não valida as alterações finais de staging/binding.

| Suíte Rust anterior | Passes | Ignored | Falhas |
| --- | ---: | ---: | ---: |
| rowd unit | 10 | 0 | 0 |
| daemon integration | 1 | 0 | 0 |
| tls_sync integration | 7 | 0 | 0 |
| v2 integration | 9 | 0 | 0 |
| rowd-android unit | 12 | 0 | 0 |
| rowd-app unit | 25 | 0 | 0 |
| rowd-core unit | 70 | 3 | 0 |
| scan_stream integration | 6 | 0 | 0 |
| rowd-daemon unit | 4 | 0 | 0 |

Doc-tests anteriores: zero casos, zero falhas. JUnit Kotlin puro anterior:
31 casos, zero falhas, 1,373 s; não foi uma execução completa da suíte Android/SAF.

Os seis testes de stream escritos na etapa anterior cobrem 1.040 arquivos com
sobreposição, queda após 500 e reconnect, mutação namespace/hash, writes internos
vs externos, colisão tardia/MAX_FILES e IDs/sequências inválidos. Os testes Kotlin
de worker cobrem a fila limitada, cancelamento e propagação de falha. Não representam
a execução dos 26 cenários SAF solicitados; essa matriz completa ainda não foi
demonstrada num provider/aparelho real.

A primeira compilação Android offline encontrou dependências ausentes do Gradle.
Depois do download, a compilação encontrou duas referências incorretas a `.target`
no retorno DocumentFile de `cachedTarget`; foram corrigidas usando `cachedSource`
com ancestralidade validada. A compilação `:app:compileReleaseKotlin` então passou.
Esse resultado é compilação de produção, não execução de testes nem validação de APK.
A última compilação offline da versão final concluiu em 1 min 51 s, com 16 tasks:
uma executada e 15 up-to-date. Logs desta etapa: `/tmp/rowd-stream-check.log` e
`/tmp/rowd-stream-kotlin-final.log`. Logs de testes anteriores:
`/tmp/rowd-rust-tests.log` e `/tmp/rowd-kotlin-standalone.log`.

A revisão também identificou que remover flags full/deep somente no commit poderia
apagar uma nova solicitação de audit recebida durante o worker. O streaming passou
a consumir essas flags no início do namespace e preservar novos pedidos, seguindo
a política de restauração de dirty/flags do scan anterior em caso de defer.

Limites ainda sem comprovação experimental: ganhos no Share real, todos os
providers SAF, cancelamento rápido de um provider bloqueado dentro de read e os
cenários de crash/reconnect/ACK perdido após as alterações finais. A ausência de
testes nesta etapa não equivale a afirmar que essas verificações passaram.
