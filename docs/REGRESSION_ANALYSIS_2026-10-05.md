# Análise de regressão — 05/10/2026

**Conclusão:** existem bugs locais independentes, mas o impacto atual é amplificado por contratos incompatíveis entre scan, defer, scheduler e observabilidade. Isso pede correções locais e uma simplificação delimitada dessas transições, não uma reescrita geral. Corrigir apenas `fromSingleUri` não recupera o funcionamento: a análise reproduziu também uma falha de desserialização de `HashStreamEnd`.

Nenhum patch funcional foi aplicado. Rust síncrono, Kotlin e SAF permanecem como premissas.

## Evidências e limites

- Histórico Git: comparação de `ba2b10c`, `cc8b11b`, `75d471a`, `ff235cb`, `170dd6d`, `c24f5d7`, `332d7f4` e estado atual.
- HEAD: `c5a6754`. O workspace anuncia `0.8.3-alpha`, protocolo V14, mas contém alterações locais extensas e arquivos não rastreados. O streaming atual e `ScanStreamMetrics` pertencem a esse conjunto local; não é correto atribuí-los a HEAD ou inventar um commit de introdução.
- PC: `/home/leite/.local/share/rowd/.rowd/Latest-trace/trace-0001.jsonl`, sessão `session-226686-1791210269243-6`, 9.285 eventos, 05/10, 14:24:29–14:31:39 UTC.
- Android: `/home/leite/Documents/rowd/doc_from_phone/8.zip`, 2.121 eventos, Infinix X666B, Android 12, 0.8.3-alpha. Exportação marcada incompleta; começa com uma rodada já em andamento e termina após iniciar outro retry.
- Repomix local: contém a mesma construção `fromSingleUri` na linha 343 de `FolderAccess.kt`.
- V4 validada em uso real e a qualidade de recuperação de `ba2b10c` são evidência histórica fornecida pelo usuário. O código confirma o mecanismo que permite essa recuperação; não foi executado novamente em Android real nesta análise.

Os relógios e IDs locais de rodada dos dois dispositivos não devem ser igualados diretamente. A correlação abaixo usa sequência de mensagens, Share e stack, sem estimar latência entre relógios.

## 1. Linha do tempo comportamental

| Estado | Alteração relevante | Propriedade e interpretação |
|---|---|---|
| 21/09, `3acfac7`, V4/0.4.0 | Fluxo completo, múltiplos Shares, persistência/recovery/SAF | Último marco amplo de uso real informado. Não equivale a ausência de defeitos. |
| 24/09, `f816edf`, 0.5.0 | Sincronização persistente e caminhos de delta focado | Conexão persistente já antecede o baseline. Não foi criada por `cc8b11b`. |
| 27/09, `86adf93`, depois `d7de4bb` | Preempção de auditoria; `round_deferred` encerra o loop de Shares | Política global de defer já existia antes do baseline, originalmente para dar prioridade a mudanças. |
| 28/09, `ba2b10c`, 0.5.3 | Baseline de recuperação indicado | Erro devolvido pela rodada descarta `CONNECTION`; tentativa seguinte cria TCP/TLS/autenticação novos. Não há callback de geração invalidando a tentativa. |
| 28/09, `cc8b11b`, 0.6.0 | Assinatura de rede ampla, callback destrutivo, gerações, clone de socket e resolver | Primeiro delta após o baseline que faz uma conexão útil depender também da estabilidade de metadados da rede. |
| 28/09, `75d471a` | Discovery por interfaces, retransmissão de consultas, cache identificado por peer/certificado/geração | Corrige disponibilidade/identidade de endpoints; mantém a política de invalidação introduzida em `cc8b11b`. |
| 29/09, `ff235cb`, 0.6.1 | Pareamento via LAN | Acrescenta estados de pairing, mas não origina o callback, resolver, geração ou fechamento de socket. |
| 29/09, `170dd6d` | Unlink unilateral e ativação de descoberta | `clearPersistentConnection` delega a `networkChanged`. Reutiliza a geração para invalidação explícita; não é a origem da regressão anterior. |
| 30/09, `c24f5d7` | `NetworkPath`, retenção de dados conhecidos, checagens de geração e limpeza externa de erros | Reduz falsos positivos de rede e cobre saídas que escapavam da limpeza. Também impõe descarte amplo em qualquer erro após adquirir a conexão. |
| 30/09–01/10, daemon/Trace v2 | Novos ciclos de execução e instrumentação | A presença dessas mudanças não prova uma regressão de transporte. É preciso distinguir eventos normais e erros reais. |
| 02/10, `332d7f4` | Classificação de falhas, EINTR, reuso condicionado à conclusão de sessão; erro SAF convertido em defer | Corrige descarte excessivo, mas erro local passa a herdar a política global de preempção. Aqui existe um acoplamento concreto entre uma correção e outra classe de falha. |
| 02/10, `c5a6754` | Idle Android por eventos e diagnósticos SAF | Estado versionado anterior ao grande conjunto local atual; não inclui a revalidação de diretórios do hash stream atual. |
| Workspace 0.8.3/V14 | Namespace/hash streaming, cache físico, staging e métricas de pipeline | Introduz o uso inválido de `SingleDocumentFile.listFiles` e `u128` em métricas transportadas por enum serde. Exerce repetidamente o acoplamento erro local → defer global. |

## 2. Onde mudou recovery/liveness

O último baseline conhecido de **recuperação automática** é `ba2b10c`. Não há evidência suficiente para chamar qualquer commit posterior de último estado globalmente saudável. V4 é o marco mais amplo de funcionamento informado, e `ba2b10c` é o baseline mais preciso para reconexão.

Em `ba2b10c`, a sequência relevante em `NativeBridge_sync` é:

1. Se não há conexão, conectar e autenticar.
2. Executar a rodada.
3. Se a rodada retorna erro, colocar `CONNECTION=None`.
4. Uma nova tentativa pode autenticar um stream novo. O conjunto de Shares ignorados existe apenas naquela chamada.

Isso não é uma prova de recuperação em toda saída possível: já havia operações com `?` fora do tratamento específico da rodada. A propriedade confirmada é a recuperação do caminho de erro da rodada descrito pelo usuário.

Em `cc8b11b`, a assinatura inclui rede, interface, lista de endereços, rotas e capacidade Wi-Fi. Alteração nessa representação chama `networkChanged`, incrementa a geração e executa `shutdown(Both)` no clone ativo. Uma tentativa autenticada também é rejeitada se a geração mudou durante sua construção.

**Contraexemplo comportamental:** LAN permanece utilizável; muda um endereço secundário ou uma rota representada na assinatura; o callback incrementa a geração; o socket útil é fechado ou a autenticação recém-concluída é descartada. Se isso se repete, as tentativas podem ser continuamente invalidadas. No baseline, essa alteração de metadados não causava essa transição.

Portanto, `cc8b11b` é a primeira mudança semântica identificada **no intervalo a partir de `ba2b10c`**: a recuperação passa a exigir uma janela estável do observador de rede, além de conectividade/autenticação possíveis. Isso confirma parte da análise anterior, mas não prova que todo caso histórico de reconexão foi causado por ela. Sem replay/traces daqueles commits, não há bisseção experimental de uma falha permanente.

Também não encontrei fundamento para “o resolver fica permanentemente envenenado”: ele tenta discovery e o endereço do convite; o endereço autenticado anterior pode continuar como candidato. Uma falha não cria um cache terminal de fracasso. Uma geração antiga, quando `CONNECTION=None`, não bloqueia por si só a próxima conexão. Endereço obsoleto e multicast indisponível podem impedir descoberta, mas são outra condição.

## 3. O que permanece da regressão antiga

- Permanece o mecanismo callback → geração → shutdown/rejeição de conexão.
- A assinatura ampla **não permanece igual**: `c24f5d7` a substitui por `NetworkPath` e elimina a sensibilidade direta a rotas/capabilities, retendo informações conhecidas quando o callback vem incompleto.
- Permanecem múltiplas representações da vida da conexão: stream persistente, clone ativo e gerações. Exigem coordenação, mas sua existência não prova falha atual.
- A política “qualquer erro descarta conexão” foi substituída por classificação e `stream_reusable`. EOF/reset/broken pipe e sessão incompleta exigem reconexão; erro local após boundary concluída pode preservar transporte.
- Mudança de rede real ainda pode abortar uma rodada, por decisão explícita. Não aparece como causa dominante na captura atual.
- Permanece um acoplamento de geração de filesystem: `WakeState.wake` incrementa uma geração global mesmo com Share específico; `FolderAccess.checkStream` compara essa geração global. Assim, uma mudança em B pode abortar o scan de A. É um risco de preempção/starvation confirmado no código, mas não a causa das dez exceções SAF observadas.

## 4. Cadeia SAF confirmada ponta a ponta

Referências: `FolderAccess.kt:150,166,232,265,341,382`; `ScanStreamWorker.kt:17,36`; `rowd-android/src/lib.rs:365,465,575`; `transport.rs:300`; `sync.rs:2484`; `rowd-app/src/lib.rs:2532`.

1. A raiz SAF é usada como árvore; `namespaceJson.walk` lista normalmente seus filhos e registra URI/prefixo de cada diretório, inclusive da raiz.
2. O worker de hash valida cada fonte e calcula/reutiliza SHA; registra evidência no cache físico.
3. A revalidação estrutural reconstrói cada diretório com `DocumentFile.fromSingleUri` e chama `listFiles`.
4. Essa factory retorna `SingleDocumentFile`, cujo `listFiles` lança `UnsupportedOperationException`. É erro de uso da API, não indicação de perda de permissão ou de conexão.
5. `FutureTask` captura a falha. `ScanStreamWorker.poll` chama `get` quando a fila esvazia, propagando `ExecutionException`; os chunks anteriores podem já ter sido entregues, mas não há fim de stream válido.
6. JNI em `AndroidStore.call` descreve a exceção e cria `LocalFilesystemError`. `poll_stream` tenta cancelamento/finalização/descarte e `deferred_scan_result` registra a causa local, devolvendo `ScanDeferred`.
7. Ao atender `HashNext`, o responder descarta o scan, envia `ScanDeferred` e retorna `round_deferred=true`.
8. O PC recebe o defer, termina o loop de Shares e envia `RoundDeferred`.
9. A sessão alcança boundary reutilizável. No Android, `finish_round` devolve o erro SAF acumulado, agora com `RoundFailure { stream_reusable: true }`.
10. A classificação é `filesystem_error`; a conexão permanece; `SyncService` tenta novamente e encontra a mesma operação impossível.

O stack do Android, evento 63, contém `SingleDocumentFile.listFiles(SingleDocumentFile.java:115)` → `FolderAccess.kt:344` → `ScanStreamWorker`; `pollHashStreamJson` aparece na linha 382. O evento 76 já informa `filesystem_error` e `stream_reusable=true`; o 84 reutiliza a conexão com ambas as gerações em 1.

São **dez exceções**, duas em `document_to_phone` e oito em `document_from_phone`. Os vinte `SAF_CALL_FAILED` são dois registros por exceção, não vinte falhas independentes. Há **onze defers**: dez dessas falhas e um defer por preempção, registrado no evento Android 1085. O PC marca os onze como sucesso.

Fonte externa de apoio: [implementação AndroidX de SingleDocumentFile](https://raw.githubusercontent.com/androidx/androidx/androidx-main/documentfile/documentfile/src/main/java/androidx/documentfile/provider/SingleDocumentFile.java). A confirmação da versão usada em execução vem do stack; o projeto declara documentfile 1.0.1. Uma correção deve preservar a identidade do subdiretório e a permissão de árvore. Não basta trocar factories indiscriminadamente sem verificar a semântica da versão 1.0.1.

## 5. Cache físico e estados de commit

O cache não é a causa desse crash. `PhysicalHashCache` separa Share/binding, URI, path, tamanho e modificação; respeita geração quando o path está dirty e ignora reuso no deep scan. O hash é lembrado depois da leitura completa, revalidação da fonte e comparação de tamanho. `save` ocorre também no `finally` do hash worker.

No Android há seis hashes calculados para os arquivos de `document_from_phone` e **42 reusos**, correspondentes a sete novas passagens pelos seis arquivos. Isso é evidência direta de reuso entre rodadas abortadas. Não demonstra por si só persistência correta após reinício do processo; a implementação grava em disco, mas esse cenário não foi exercitado pela captura.

`discardScanJson` remove staging/pending scan e restaura dirty flags pertinentes; não apaga o cache físico. `pendingScan` só é preparado após as revalidações finais. `commitScanJson` instala cache de scan/índices e `scanReady`, não a base de reconciliação. Em Rust, `coordinate_with_progress_and_audit_control` trabalha sobre uma cópia do estado; defer não promove essa cópia. A base em disco usa candidato `.next`, marcador `.pending` e caminho de conclusão separado.

Três conceitos precisam continuar distintos: hash físico, snapshot de scan aceito e base protocolar. O streaming permite progresso físico e ACKs de operações individualmente validadas antes de terminar toda a rodada; portanto “rodada abortada não fez nada” seria uma descrição incorreta. A garantia necessária é não promover uma base parcial como completa nem tratar hash cacheado como ACK/existência.

Metadados iguais não são prova universal de bytes iguais: o reuso depende da política de dirty tracking e deep audit para provedores que não sinalizam bem modificações. Isso é limite da política de cache, não justificativa para eliminá-lo nem explicação da exceção atual.

## 6. Starvation e observabilidade

O PC apresenta uma só conexão nos onze `ROUND_END`: `connection-226686-1791210241284-4`. O Android registra onze `CONNECTION_REUSED`, sem evento de limpeza ou troca de geração na janela. Isso sustenta que o transporte permanece útil entre retries; não exige reinterpretar falha SAF como rede.

Nas oito rodadas completas com seis/sete Shares, o PC seleciona `document_from_phone` como primeiro Share e defere antes de selecionar o próximo. O trecho decisivo é `if report.round_deferred { ... break; }`. `managed.rs` recebe `RoundDeferred` e limpa a fila daquela rodada. O erro é reapresentado somente no fim da sessão Android, quando `failed_share` já foi limpo; o mecanismo de excluir um Share com falha não resolve esse caso.

Isso comprova bloqueio dos sucessores na captura e possibilidade de starvation se a ordem e a falha se repetirem. Não significa que nenhum Share possa avançar em qualquer rodada focada ou reordenada.

**Não é necessário invalidar todos os Shares para preservar integridade.** É necessário respeitar a boundary combinada pelos peers. Hoje ambos entendem o defer como saída global; retirar apenas o `break` seria arriscado. A solução mínima pode concluir a rodada atual e agendar os demais, isolando o Share problemático. Outra opção é um resultado explícito de Share com continuação acordada. Ambas precisam preservar descarte de staging, consumo de controle pendente e alinhamento de frames.

Há retry com espera limitada a 60 segundos, não um estado terminal por Share. A espera pode ser omitida se há dirty pendente. O Android mostra erro na UI, portanto o loop não é totalmente silencioso; o PC, porém, mostra sucesso e mensagem de auditoria adiada para priorizar mudança do PC até quando a causa é SAF local.

Telemetria recomendada: `result=success|deferred|failed`, acompanhada de `protocol_completed`, `stream_reusable`, causa, Share responsável, Shares concluídos/adiados e repetição sem progresso. Enquanto a causa não viaja no wire, o PC pode afirmar `deferred`, mas não inventar `filesystem_error` nem “mudança do PC”. Encerrar o defer corretamente não é sincronizar corretamente.

## 7. Ruído do listener

Os **7.581 eventos** são todos `operation=accept`, `error_kind=WouldBlock`, `errno=11`: **81,6478%** da captura PC. Não são perdas de conexão.

`rowd-app/src/lib.rs:3207` usa `io_retry::interrupted` para listener não bloqueante; o caller já trata `WouldBlock` como idle. `io_retry.rs:19` oferece `poll`, que preserva o retorno, repete EINTR e suprime apenas o registro de idle esperado. O daemon tem o mesmo padrão em `unix.rs:581`. É escolha errada de wrapper, corrigível localmente, sem alterar política de reconexão.

## 8. Outro bloqueio atual, descoberto nesta análise

O teste existente `thousand_files_overlap_hash_and_transfer_and_commit_once` falha com **`u128 is not supported`**. Um reproducer mínimo, compilado contra as dependências atuais, confirmou:

```text
serde_json::to_string(Message::HashStreamEnd { métricas padrão }) → OK
serde_json::from_str::<Message>(json) → Err("u128 is not supported")
serde_json::from_str::<ScanStreamMetrics>(json_das_métricas) → OK
```

`Message` é um enum internamente marcado (`#[serde(tag = "type")]`, `protocol.rs:18`); `HashStreamEnd` contém `ScanStreamMetrics`, com `namespace_ms`, `queue_wait_ms` e tempo opcional em `u128` (`storage.rs:61`). A combinação de desserialização desse enum com esses campos é incompatível no conjunto atual de dependências. Até métricas zero reproduzem a falha; não é overflow nem lentidão da máquina.

É um bug novo do workspace V14, independente de SAF, cache e rede. As métricas passaram a integrar uma mensagem necessária ao progresso do protocolo. Corrigir SAF permite chegar mais longe, mas deixa esse bloqueio na conclusão do hash stream. **Não apareceu no trace atual porque o SAF falha antes de produzir `HashStreamEnd`.**

Validação executada: quatro testes de `scan_stream` passaram. Dois testes com sockets foram inicialmente bloqueados pelo sandbox; repetidos fora dele, o de 1.040 arquivos falhou como acima, confirmado também isoladamente. O teste de perda após 500 arquivos permaneceu em execução por mais de 60 segundos e a suíte foi interrompida; não há resultado conclusivo desse teste. Não se deve registrá-lo como aprovado nem afirmar a causa do bloqueio sem investigação própria. Nenhum teste Android em aparelho foi executado nesta sessão.

## 9. Árvore causal e classificação

```text
Recuperação histórica
  assinatura ampla de rede [cc8b11b]
    → mudança sem perda comprovada de LAN
    → geração + shutdown/rejeição
    → tentativas invalidadas enquanto eventos persistem
    → mitigação em c24f5d7; mecanismo restrito permanece

Falha atual dominante
  revalidar árvore reconstruída como documento individual [workspace V14]
    → UnsupportedOperationException [causa local]
    → worker/JNI LocalFilesystemError [propagação]
    → ScanDeferred [recuperação segura do protocolo, 332d7f4 + streaming]
       → preserva conexão [comportamento desejado]
       → usa semântica global de preempção [acoplamento]
          → break + RoundDeferred + fila descartada [consequência]
          → sucessores sem processamento [starvation]
          → retry sem isolamento/terminal por Share [amplificação]
       → PC Ok(()) → ROUND_END success [erro de observabilidade]
    → hashes completos salvos/reusados [otimização válida]

Outro bloqueio após resolver SAF
  métricas u128 em HashStreamEnd internamente marcado
    → falha de decode [causa protocolar independente]
    → stream não conclui; base não deve ser promovida

Ruído independente
  interrupted(accept) em polling não bloqueante
    → WouldBlock registrado como IO_SYSCALL_FAILED
    → 7.581 eventos sem falha real de conexão
```

## 10. Invariantes que devem orientar a recuperação

| Invariante | Estado/ação necessária |
|---|---|
| Erro local não corrompe outros Shares | Separação de base não basta; garantir também progresso dos demais. Hoje o defer global impede isso em certas filas. |
| Scan abortado não promove base parcial | Preservar candidato/pending e distinguir `commit_scan` de commit da base. Testar abortos após chunks e operações físicas. |
| Conexão reutilizada somente após boundary segura | O caminho observado cumpre; erro de filesystem fora de boundary pode exigir fechar mesmo com socket fisicamente saudável. |
| Conexão quebrada nunca é reutilizada | Preservar descarte por EOF/TLS/reset, sessão incompleta e geração efetivamente invalidada. |
| Retry progride ou expõe bloqueio por Share | Falta isolamento de falha determinística e política explícita de repetição sem progresso. |
| Polling idle não é falha | Violado nos listeners; usar semântica de polling existente. |
| Hash físico pode sobreviver sem commit | Confirmado no código e no reuso entre retries; preservar. |
| Mensagem emitida pode ser lida pelo peer da mesma versão | Violado por `HashStreamEnd`/`u128`; testar o codec da mensagem inteira. |
| Evento em B não invalida evidência de A sem necessidade | Geração global torna a política mais abrangente que dirty tracking por Share. Delimitar validade e prioridade separadamente. |
| Telemetria não confunde defer com sync concluído | Violado no PC; causa e resultado precisam sobreviver às camadas. |

## 11. Ordem de correção por dependência

1. **Fixar os contratos de resultado e as evidências de reprodução.** Adotar distinção entre causa, boundary reutilizável, resultado do Share e resultado da rodada. Guardar as duas capturas atuais e um snapshot identificável do workspace; não chamar este estado apenas de `c5a6754`.
2. **Remover os dois bloqueios determinísticos do full scan:** enumeração SAF com identidade correta de diretório e codec de `HashStreamEnd`. São independentes, mas ambos são pré-requisitos de uma rodada completa. Preservar cache físico, verificação estrutural e proteção da base.
3. **Separar falha local de preempção global no agendamento.** Preservar a boundary atual e dar oportunidade aos demais Shares; retry/estado bloqueado precisa pertencer ao Share problemático. Não trocar `break` por `continue` sem verificar os dois peers.
4. **Tornar resultados observáveis e eliminar idle falso.** O campo `deferred` e a supressão de `WouldBlock` podem ser entregues antes/em paralelo aos passos anteriores; são suporte à validação, não solução para liveness. Preservar a causa local no resumo PC exige acordo de protocolo se ela for transmitida.
5. **Revalidar lifecycle de transporte e escopo de gerações.** Testar EOF/TLS/reset seguido de reconexão; mudança real de rede; callback sem perda da LAN; eventos em outro Share durante scan. Só simplificar o que violar esses contratos.
6. **Fechar a validação em Linux + Android real.** Executar Share vazio e aninhado, arquivos maiores que um chunk, dois sentidos, edição durante scan, erro SAF em A com B/C saudáveis, desconexão durante transferência, retry com hashes reusados e reinício do processo. Verificar bytes e base final, oportunidade de todos os Shares e ausência de sucesso falso.

Critério de decisão A/B: A explica os bugs SAF, codec e wrapper de polling. B explica o acoplamento de uma falha local a defer global e retry sem progresso. A resposta sustentada pelas evidências é **bugs locais mais uma simplificação arquitetural localizada do tratamento de resultados e agendamento**, sem evidência para uma refatoração ampla ou introdução de Tokio.
