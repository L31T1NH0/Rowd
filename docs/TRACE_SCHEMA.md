# Rowd Trace v2

Trace é diagnóstico profundo **opt-in**. `daemon.log` mantém logs operacionais e `events.jsonl` mantém os eventos públicos. JSONL é a fonte de verdade; a apresentação humana é derivada.

## Uso

```bash
rowd daemon trace start
rowd daemon trace start --output-dir /caminho/absoluto
rowd daemon trace status
rowd daemon trace flush
rowd daemon trace stop
rowd run --trace
rowd trace show /caminho/Latest-trace
rowd trace show /caminho/Latest-trace --component watcher --share camera
rowd trace show /caminho/Latest-trace --errors
rowd trace show /caminho/Latest-trace --connection connection-id
rowd trace show /caminho/Latest-trace --round round-id
rowd trace show /caminho/Latest-trace --file file-id
rowd trace show /caminho/Latest-trace --request request-id
```

O diretório padrão no PC é `<ROWD_HOME>/.rowd`. `--output-dir` exige caminho absoluto. Logs/events, inclusive follow, permanecem disponíveis. No Android, os controles e a exportação do trace de desempenho continuam disponíveis; a raiz é `<filesDir>/diagnostic-trace`. Kotlin envia eventos estruturados via JNI ao **mesmo writer Rust**, com a mesma sessão, sequência, relógio monotônico e chunks. O ZIP exportado contém metadata e JSONL de Latest-trace.

## Persistência

```text
<raiz>/
  Latest-trace/
    metadata.json
    trace-0001.jsonl
    trace-0002.jsonl
  traces/
    trace-<trace_session_id>/
      metadata.json
      trace-0001.jsonl
```

Cada evento é serializado e escrito diretamente em um File sob um mutex. Não há fila descartável, buffer da sessão inteira ou rotação destrutiva. `CHUNK_BYTES` centraliza o limite de 64 MiB. Um evento indivisível maior que o limite ocupa um chunk próprio.

Uma thread faz flush e sync_data aproximadamente a cada segundo. ERROR, INVARIANT_VIOLATION e ROUND_END sincronizam imediatamente. Escritas sobrevivem à morte do processo; uma queda de energia pode perder escritas ainda não sincronizadas conforme filesystem/dispositivo. Um kill durante uma escrita pode deixar a última linha parcial; o renderer indica INCOMPLETE_FINAL_RECORD sem esconder corrupção no meio do arquivo.

Stop bloqueia novas emissões, espera o mutex, sincroniza o arquivo e grava metadata concluída. O histórico é copiado para um diretório temporário, sincronizado e publicado por rename. Arquivos históricos são somente leitura; a aplicação não os altera. Latest-trace permanece. Antes de outra sessão, a anterior é arquivada. Sessões interrompidas preservam complete=false, ganham recovered=true e termination=recovered_after_unclean_exit. Falha de leitura/cópia impede o reset e preserva a evidência.

Permissões Unix: diretórios privados 0700 e chunks 0600. Uma trava de arquivo por raiz impede dois processos de sobrescreverem a mesma sessão; o daemon também possui seu lock de instância. Falhas do writer desativam a coleta e aparecem no status, stderr/Logcat e daemon.log, mantendo a aplicação funcionando. Metadata recebe o erro quando ainda pode ser escrita. Não há descarte silencioso.

## Schema do evento

| Campo obrigatório | Tipo | Significado |
| --- | --- | --- |
| schema_version | integer | Sempre 2 |
| seq | integer | Contínuo e crescente desde 1 por sessão |
| wall_time | string | ISO 8601 UTC, milissegundos e sufixo Z |
| wall_ms | integer | Unix epoch em milissegundos |
| elapsed_us | integer | Instant monotônico desde ativação |
| level | string | trace, debug, info, warn ou error |
| side | string | pc, daemon ou android |
| component | string | Componente padronizado |
| event | string | Nome fixo UPPER_SNAKE_CASE |
| source | object | file, line, function, thread e pid |
| context | object | IDs herdados de correlação |
| fields | object | Dados específicos do evento |

```json
{"schema_version":2,"seq":93,"wall_time":"2026-10-01T23:45:01.884Z","wall_ms":1790898301884,"elapsed_us":38199217,"level":"error","side":"daemon","component":"Daemon-IPC","event":"IPC_WRITE_FAILED","source":{"file":"crates/rowd-daemon/src/unix.rs","line":120,"function":"rowd_daemon::unix","thread":"unnamed:ThreadId(4)","pid":812},"context":{"trace_session_id":"session-812-...","process_instance_id":"process-812-...","daemon_instance_id":"process-812-...","request_id":"request-812-...","command":"status"},"fields":{"error":{"kind":"daemon_ipc","code":"broken_pipe","operation":"send_status_response","message":"Broken pipe (os error 32)","chain":["Broken pipe (os error 32)"],"os_kind":"BrokenPipe","os_code":32,"os_message":"Broken pipe (os error 32)"}}}
```

wall_ms pode saltar após ajuste do relógio civil. Ordene por seq dentro da sessão, e use elapsed_us para durações. Correlação entre dispositivos depende dos relógios e não prova causalidade. O renderer apresenta a hora UTC do JSON:

```text
23:45:01.884 [ERROR] [Daemon-IPC] [req=request-...] IPC_WRITE_FAILED | error={...} | source=crates/rowd-daemon/src/unix.rs:120 ...
```

Rust usa trace_event! e trace_legacy_event! para file!, line! e module_path!. source.function é o módulo Rust, não necessariamente o nome da função. event() mantém track_caller para compatibilidade. Kotlin informa arquivo, função/contexto e thread, sem stacks em eventos normais; line=0 significa indisponível.

## Componentes e contexto

Componentes: CLI, Terminal-output, Daemon, Daemon-IPC, Android-Service, Watcher, Scanner, Scheduler, Round, Discovery, Network, Connection, Protocol, Heartbeat, Transfer, Filesystem, SAF, StateStore, Pairing, Recovery, Trace.

TraceContext herda campos por escopos RAII, restaurando o contexto anterior ao sair. Workers recebem explicitamente uma cópia; não há propagação mágica entre threads. JNI síncrono conserva o contexto Rust nos callbacks Kotlin de SAF.

IDs: trace_session_id, process_instance_id, daemon_instance_id, request_id, connection_id, connection_attempt_id, round_id, share_id, file_id e transfer_id. Também share_name, share_index e share_total quando conhecidos. Campos indisponíveis são omitidos. Os IDs de operações são locais a cada dispositivo: o protocolo de sync não foi alterado para transportá-los entre dispositivos. CONNECTION_ESTABLISHED vincula a tentativa autenticada à conexão persistente.

file_id permanece hex(SHA256(share_id + NUL + relative_path)[0..8]), compatível Kotlin/Rust. transfer_id vincula preparação, blob, instalação e confirmação por arquivo e rodada. Confirmações em batch também possuem evidência por arquivo. A unidade atual é o arquivo na rodada; suboperações de conflito do mesmo arquivo podem compartilhar esse ID.

## Eventos

Lifecycle: TRACE_START/STOP, TRACE_PRODUCER_STOP, PROCESS_START/STOP, DAEMON_START/READY/STOP, ANDROID_SERVICE_CREATE/START/STOP/DESTROY, WORKER_START/STOP/INTERRUPTED.

Watcher: OBSERVER_REFRESH_START, OBSERVER_REGISTER_START/REGISTERED/REGISTER_FAILED, OBSERVER_UNREGISTERED, OBSERVER_CALLBACK, OBSERVER_CHANGE_CLASSIFIED, WATCHER_WAKE_REQUESTED e WATCHER_QUEUE_OVERFLOW. Falhas de registro preservam o fallback full_audit e suas causas.

Scheduler: ROUND_CREATED/START/END, SHARE_CONSIDERED/SELECTED/SKIPPED/FAILED e FILE_RECONCILE_DECISION. Decisões registram reason, foco, disponibilidade, política de direção e hashes/estado relevantes, sem modificar a ordem ou algoritmo existente.

Scanner/filesystem: FILE_FIRST_SEEN, FILE_ENUMERATED, FILE_HASH_START/END/REUSED, AUDIT_SCHEDULED/START/END, DEEP_AUDIT_START/END, DELTA_UNAVAILABLE, DELTA_SCAN_START/END, FULL_SCAN_FALLBACK, SNAPSHOT_START/END e INSTALL_START/END. Os eventos atuais de scan, manifest e cache continuam normalizados para maiúsculas.

Rede: NETWORK_CALLBACK/CHANGED, DISCOVERY_START/QUERY_SENT/REPLY_RECEIVED/CANDIDATE/CANDIDATE_REJECTED/END/FAILED/CACHE_REJECTED, CONNECTION_ATTEMPT/ATTEMPT_FAILED/ESTABLISHED/REUSED/CLEARED/CLOSED, SOCKET_CONNECT_START/END, TLS_HANDSHAKE_START/END, AUTH_START/SUCCESS/FAILED e POLL_WAKE_START/RESULT/EOF/ERROR. TLS permanece lazy: início indica preparação e fim é observado na autenticação bem-sucedida. O protocolo atual não possui heartbeat explícito; não são fabricados HEARTBEAT_SENT/RECEIVED/TIMEOUT para operações inexistentes.

Protocolo/transfer: PROTOCOL_SEND/RECEIVE e falhas, TRANSFER_QUEUED/START/COMPLETE/FAILED, BLOB_SEND_START/END, BLOB_RECEIVE_START/END e REMOTE_ACK. Só tipos, tamanhos e identificadores, nunca payloads.

StateStore/SAF: STATE_PERSIST_START/END/FAILED e SAF_CALL_START/END/FAILED. IPC: IPC_ACCEPT, IPC_REQUEST_RECEIVED/PARSED, IPC_RESPONSE_START/SENT, IPC_CLIENT_CLOSED, IPC_READ_FAILED/WRITE_FAILED/TIMEOUT/HANDLER_FAILED. IPC_CLIENT_CLOSED indica fim do handler, não prova recebimento pelo cliente. A resposta de trace_stop ocorre depois da finalização e, portanto, fora da sessão encerrada.

RUNTIME_STATE_SNAPSHOT ocorre aproximadamente a cada 30 segundos quando ativo. Android inclui serviço, worker, observers, Share selecionada, dirty/pending URIs, rede, conexão, gerações e cancelamento. Daemon inclui readiness, conexão, uptime e handlers. INVARIANT_VIOLATION produz evidência e não executa recuperação automática. Terminal-output registra mensagens sem capturar frames da TUI.

## Primeira observação

FILE_FIRST_SEEN ocorre uma vez por file_id por sessão. source em fields distingue watcher, audit_scan, startup_scan, manual_scan e remote quando conhecidos. Campos indisponíveis são null. Android inclui idade do processo/observer e se havia observer ativo.

metadata_created_ms, metadata_modified_ms e provider_last_modified_ms **não provam a criação real**. file_age_at_first_observation_ms é a diferença entre relógio civil e metadata disponível, podendo ser negativa por divergência de relógios. Não é watcher_delay.

## Metadata e erros

Metadata contém trace_schema, trace_session_id, rowd_version, platform, side, process_instance_id, daemon_instance_id quando aplicável, started_at, finished_at, complete, recovered e termination. PC inclui os, architecture, pid e launch_mode. Android acrescenta android_version, device_model e app_version ao iniciar a sessão. Finalização também inclui event_count e chunk_count.

PROCESS_START Android emitido ao ativar a coleta tem observed_via_trace_activation=true e process_started_wall_ms com a origem temporal do processo. Desativar o trace em um processo vivo emite TRACE_PRODUCER_STOP, não um PROCESS_STOP fictício.

termination: trace_stop, process_exit, daemon_stop, service_stop, recovered_after_unclean_exit ou unknown. Sessões interrompidas mantêm finished_at=null.

Erros Rust preservam a chain anyhow, incluindo ErrorKind, errno e mensagem OS. Famílias: network, discovery, connection, handshake, watcher, filesystem, state_store, daemon_ipc, protocol, transfer, scheduler, android_service e trace. Codes IPC incluem socket_not_found, connection_refused, broken_pipe, permission_denied, timeout, invalid_response, unexpected_eof e io_error. Kotlin preserva classe, message, causas e stack em ERROR. WARN de fallback conserva causas sem capturar stack normal.

## Privacidade

Nunca envie secrets, chaves privadas, convites/QR privados, credenciais, blobs ou conteúdo de arquivos aos helpers. O writer oferece defesa adicional: redaction recursiva de chaves sensíveis, valores registrados como secrets e tokens rowd1:. Isso não substitui o cuidado nos callsites.

**Traces são material sensível:** contêm caminhos relativos, nomes de arquivos/Shares, hashes, IPs e erros. Inspecione antes de compartilhar. Não há retenção destrutiva automática; sessões longas e históricos podem ocupar muito disco.

## Validação

Testes cobrem schema, seq/elapsed monotônicos, source, contexto, erros, redaction, ponte Kotlin/JNI, persistência imediata, chunks, finalização, recovery e falha do writer. Volume usa quatro produtores concorrentes e milhares de eventos. O teste CLI controla um daemon temporário, valida o renderer, mata esse daemon com SIGKILL e verifica recuperação na sessão seguinte.

```bash
cargo test --workspace -- --test-threads=1
cargo fmt --all -- --check
# Com JDK/SDK configurados:
gradle -p android testDiagnosticUnitTest compileDiagnosticKotlin
```

A suíte não substitui soak tests de várias horas em aparelho real, especialmente para LMK, providers SAF específicos e disco cheio real.
