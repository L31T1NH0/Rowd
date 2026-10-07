# Análise técnica da regressão de reconexão

## Escopo e conclusão

Análise estática dos commits `ba2b10c`, `cc8b11b`, `75d471a`, `ff235cb` e `170dd6d`, sem execução em dispositivo. **`ba2b10c` é o último baseline informado com recuperação automática; `cc8b11b` é o primeiro commit que altera a semântica relevante.** A hipótese de que `ff235cb` introduziu o mecanismo de reconexão é rejeitada: ele acrescenta pairing discovery e altera a entrada de autenticação no servidor, mas o callback de rede, as gerações, o fechamento de socket e o `EndpointResolver` já estavam em `cc8b11b`.

**Culpado mais provável:** interação, introduzida em `cc8b11b`, entre uma assinatura de rede excessivamente sensível, `networkChanged()` destrutivo e a rejeição de conexões quando a geração muda. **Confiança moderada** quanto à origem da regressão semântica; **baixa a moderada** quanto a ser a explicação completa do episódio no dispositivo. O código não demonstra que um único callback deixe a conexão permanentemente presa: após estabilização da rede e com endpoint alcançável, as tentativas devem voltar a funcionar. A persistência observada exige callbacks recorrentes, endpoint indisponível, parada do serviço, ou outra condição não comprovada por este histórico.

## 1. Baseline: `ba2b10c`

Em `crates/rowd-android/src/lib.rs` de `ba2b10c`, `CONNECTION` guardava apenas `(chave, ClientStream)` (`:21-26`). `sync()` obtinha o mutex, descartava a conexão se convite/dispositivo mudassem (`:441-448`) e, quando `None`, fazia `tls::connect(&invite)` ao endereço do convite, `client_auth` e armazenava o stream (`:451-460`). A rodada usava `managed::client_round_on_excluding` (`:462-470`). Se ela retornasse erro, `CONNECTION` virava `None` **antes** de escolher entre tentar outro Share e devolver o erro (`:481-490`). EOF, erro TLS e falha de envio/recepção dentro da rodada seguiam esse mesmo caminho. `pollWake()` também convertia EOF/erro em `"!"` e descartava `CONNECTION` (`:523-567`).

Uma falha de **estabelecimento** (`tls::connect` ou `client_auth`) ocorria antes da atribuição a `CONNECTION`; portanto ela já permanecia `None`. No `SyncService`, o erro de `NativeBridge.sync()` era capturado, `failures` aumentava e o worker esperava até `min(60 s, 5 s × failures)` antes de repetir; um `wake()` podia antecipar a espera. O serviço não saía por um erro comum (`SyncService.kt` em `ba2b10c`, aproximadamente `:232-260`). Convite, identificação, preferências, estado dos Shares e `BASE_TOKENS` sobreviviam; o stream TCP/TLS não. Essa separação permitia autenticação nova na tentativa seguinte, desde que o endereço do convite continuasse alcançável. O baseline comprova a lógica de recuperação, não uma garantia para queda longa, IP alterado ou serviço encerrado pelo Android.

`crates/rowd-core/src/managed.rs:161-183` continua enviando `StartRound` e recebendo a configuração via stream fornecido pelo chamador. Não há ali um estado global de reconexão. `client_round()` (`:124-151`) cria conexão por chamada, mas **não é o caminho da conexão persistente Android**; este usa `client_round_on_excluding`.

## 2. Diferenças cronológicas e diff causal

| Intervalo | Mudança relevante para reconexão |
| --- | --- |
| `ba2b10c..cc8b11b` | `SyncService.observeNetwork()` e `networkSignature`; JNI `networkChanged()`; globais `RESOLVER`, `NETWORK_GENERATION`, `CONNECTION_GENERATION`, `ACTIVE_SOCKET`; seleção e autenticação de endpoint antes da rodada; descarte por geração. **Primeira alteração causal.** |
| `cc8b11b..75d471a` | Ajusta disponibilidade do responder no PC e protocolo de discovery; passa a associar o cache de endpoint a par/certificado e geração. Mantém a arquitetura de geração e fechamento. Pode afetar a localização do PC, mas não introduz a nova semântica de invalidação. |
| `75d471a..ff235cb` | Acrescenta pairing discovery, APIs JNI de pairing e uma bifurcação `PairRequest`/autenticação na sessão PC (`rowd-app/src/lib.rs`). Não modifica `SyncService.observeNetwork()`, `networkChanged()` nem o trecho de `sync()` que valida gerações. Possível efeito indireto na autenticação do servidor, sem evidência de que seja o primeiro causador da reconexão. |
| `ff235cb..170dd6d` | Corrige ativação do pairing e desvinculação unilateral; acrescenta `clearPersistentConnection()` para desvinculação e pequenos ajustes no serviço. O caminho de reconexão normal e o callback permanecem. |

Trechos decisivos do diff `ba2b10c..cc8b11b`:

```text
SyncService.kt: + signature() = "$network|${links?.interfaceName}|${links?.linkAddresses}|${links?.routes}|${caps?.hasTransport(WIFI)}"
SyncService.kt: + onAvailable/onLost/onCapabilitiesChanged/onLinkPropertiesChanged -> changed()
SyncService.kt: + changed(): se assinatura diferente, networkChanged(); wake()
rowd-android/src/lib.rs: + NETWORK_GENERATION.fetch_add(1)
rowd-android/src/lib.rs: + ACTIVE_SOCKET.take().shutdown(Both)
rowd-android/src/lib.rs: + connection().try_lock(); se possível, take().shutdown(Both)
rowd-android/src/lib.rs: + descartar CONNECTION quando CONNECTION_GENERATION != NETWORK_GENERATION
rowd-android/src/lib.rs: - tls::connect(&invite); client_auth(...)
rowd-android/src/lib.rs: + resolver.connect(..., generation, discovery)
rowd-android/src/lib.rs: + ensure!(generation == NETWORK_GENERATION, "Rede alterada durante a conexão")
```

As localizações atuais são `SyncService.kt:68-94`, `NativeBridge.kt:7`, `rowd-android/src/lib.rs:23-26,175-183,235-250,664-749` e `rowd-core/src/discovery.rs:313-402`. `cc8b11b` acrescentou também o responder de discovery no PC em `rowd-app/src/lib.rs::serve`; `75d471a` o adaptou para interfaces IPv4 disponíveis. Essas são mudanças de disponibilidade de endpoint, separadas da invalidação da conexão Android.

## 3. Sequência Android → Rust → próxima rodada

1. `onCreate()` registra `registerDefaultNetworkCallback` e guarda uma assinatura calculada de `activeNetwork`, nome da interface, endereços, rotas e booleano de Wi-Fi (`SyncService.kt:68-93,107-109`).
2. Cada `onAvailable`, `onLost`, `onCapabilitiesChanged` e `onLinkPropertiesChanged` recalcula **o estado global atual**, não apenas o `Network` entregue no callback. Se a string difere, chama `NativeBridge.networkChanged()` e `wake()` (`:77-90`).
3. `networkChanged()` incrementa `NETWORK_GENERATION`, retira o clone de `ACTIVE_SOCKET` e executa `shutdown(Both)`. Tenta retirar `CONNECTION` com `try_lock`; se `sync()`/`pollWake()` segura o mutex, essa retirada é omitida (`rowd-android/src/lib.rs:235-250`). O fechamento do clone afeta o socket da conexão em uso.
4. Na entrada seguinte em `sync()`, se existir conexão e `CONNECTION_GENERATION != NETWORK_GENERATION`, `clear_connection()` a descarta (`:664-676`). Sem conexão, captura a geração, bloqueia `RESOLVER` e executa `resolver.connect()`: endpoint em cache, discovery e endereço do convite, com autenticação TLS e Rowd (`:680-703`; `discovery.rs:319-402`).
5. Uma geração diferente ao voltar de `resolver.connect()` devolve erro **antes** de armazenar o stream (`:704-707`). Após armazenar, há segunda validação; se mudou, limpa e falha (`:708-718`). A falha sobe para o `catch` do `SyncService`; este espera e tenta outra rodada (`SyncService.kt:265-303`).

### Callbacks sem troca efetiva de rede

**Sim, a aplicação pode invalidar sem troca do caminho LAN útil.** `onCapabilitiesChanged` e `onLinkPropertiesChanged` são callbacks de mudanças de propriedades; `networkSignature` inclui a lista completa de rotas e endereços. Uma atualização de rota, IPv6, DHCP ou representação temporariamente `null` pode mudar a string mantendo o mesmo Wi-Fi e o mesmo IP do PC. `onAvailable` e `onLost` também chamam `changed()` mesmo para o `Network` que não seja, naquele instante, o `activeNetwork`; só a comparação da assinatura evita invalidar. Se o estado consultado estiver temporariamente `offline` ou com propriedades incompletas e depois se completar, há duas invalidações. A assinatura igual suprime callbacks duplicados; portanto **callback isolado com assinatura idêntica não fecha o socket**. O histórico demonstra a sensibilidade e a janela de corrida, não demonstra a frequência real desses eventos no aparelho.

Sequência capaz de produzir tentativas repetidamente interrompidas:

```text
rodada ou pollWake em conexão persistente
→ assinatura muda por propriedade transitória; geração N→N+1; socket fechado; wake()
→ rodada falha/EOF; CONNECTION é limpa, ou permanece até pollWake/próximo sync
→ sync captura N+1; resolver autentica um endpoint
→ outro callback muda a assinatura; geração N+1→N+2
→ validação rejeita a conexão; serviço registra erro e agenda retry
→ novas mudanças durante cada tentativa repetem a rejeição
```

Se os callbacks cessarem, o código não contém um latch de geração que impeça a próxima conexão: o novo `sync()` lê a geração atual. Logo, **não é correto atribuir uma desconexão permanente a apenas dois callbacks**. A cadeia acima explica um loop enquanto a assinatura oscilar, e também por que a rodada pode ser interrompida mesmo quando o transporte LAN ainda funcionaria.

## 4. Estados após erros e efeito do reinício

| Estado | Comportamento verificado |
| --- | --- |
| `CONNECTION` | EOF/erro em `pollWake()` chama `clear_connection()` (`rowd-android/src/lib.rs:779-824`); erro retornado por `client_round_on_excluding` também limpa (`:722-745`). Erro de `resolver.connect()` deixa `CONNECTION=None`. Existe uma janela menor para erro depois da autenticação e antes de `CONNECTION=Some` (`authenticatedAddress`, `try_clone`): o stream local cai ao sair, e a próxima chamada recomeça. |
| `ACTIVE_SOCKET` | Clone do socket instalado após autenticação (`:710-714`); limpo quando `CONNECTION` é limpa (`:175-179`) ou retirado e fechado por `networkChanged()` (`:235-250`). Se o mutex de `CONNECTION` está ocupado, o callback não espera, mas já fecha `ACTIVE_SOCKET`; o próximo erro, `pollWake()` ou teste de geração faz a limpeza lógica. |
| `CONNECTION_GENERATION` | Atualizada quando o endpoint é autenticado (`:709`); não volta a zero ao descartar a conexão. Com `CONNECTION=None`, esse valor antigo não impede nova tentativa. |
| `NETWORK_GENERATION` | Monotônica durante o processo; cada assinatura diferente a incrementa. A mudança durante `resolver.connect()` invalida **a tentativa**, não o resolver permanentemente. |
| `RESOLVER` | `OnceLock<Mutex<EndpointResolver>>` sobrevive a erros e ao reinício do serviço no mesmo processo. Guarda último endpoint autenticado e geração. Um erro não apaga o último endpoint, mas a tentativa seguinte continua fazendo discovery e fallback. |
| `CANCELLED` | Só muda em `cancel()`/`resetCancellation()` (`:625-632`). `SyncService.onStartCommand()` chama reset (`:131`); `STOP`, timeout e `onDestroy()` chamam cancel (`:118-124,320-329`). Falha comum de transporte não seta `CANCELLED`. Se o serviço foi encerrado, o novo serviço limpa esse flag. |

O `SyncService` mantém `active=true` em erro comum, incrementa `failures` e repete. **Parar e iniciar** recria worker e contadores (`failures`, auditoria), chama `resetCancellation()` e registra novo callback (`SyncService.kt:107-145,265-329`). `onDestroy()` cancela o worker. Isso pode quebrar uma sequência de timing ruim ou remover `CANCELLED=true` de um encerramento anterior. **Não reinicia necessariamente as globais Rust**: `RESOLVER`, `NETWORK_GENERATION`, `CONNECTION` e `ACTIVE_SOCKET` pertencem à biblioteca no processo. Só um processo Android novo as reinicia; e `onCreate()` por si só não chama `networkChanged()`. Portanto a recuperação manual **não prova** que o cache do resolver seja a causa. Também pode apenas permitir nova tentativa em rede já estável.

## 5. `EndpointResolver` e endereço

Em `75d471a`, o cache passou a registrar par/certificado, endereço e geração (`discovery.rs:306-317,344-354,389-396`). “Autenticado” aqui significa que o endpoint só entra no cache depois de TLS com certificado fixado e `client_auth` bem-sucedidos (`:319-332,389-399`). Um anúncio UDP não é autoridade de confiança por si só. Geração diferente impede a tentativa rápida inicial ao cache; o endereço anterior ainda entra na lista depois dos resultados de discovery (`:346-374`). Geração igual tenta o cache rapidamente (800 ms); se falha, segue discovery e convite. `Invitation.address` sempre entra na lista, deduplicada (`:374-383`). O resultado de discovery pode falhar silenciosamente, mas não remove o fallback (`:366-374`).

Falha em todos os candidatos mantém o último endereço autenticado (`:397-401`), sem “cache de falha”. Isso **pode aumentar latência** e repetir um endereço antigo a cada tentativa, sobretudo quando o convite também contém IP antigo; não bloqueia um endpoint novo que o discovery devolva e autentique. Se o IP do PC mudou, multicast está indisponível e `Invitation.address` está obsoleto ou `0.0.0.0`, não há candidato alcançável. Esse caso pode parecer permanente enquanto as condições persistirem; reiniciar o serviço não conserta, por si só, um discovery PC indisponível. `75d471a` alterou como o PC entra nos grupos multicast por interface (`rowd-app/src/lib.rs::discovery_responder`), portanto falha real de discovery é hipótese alternativa, especialmente após mudança de IP.

## 6. Comparação das estratégias, sem implementação

| Estratégia | Alcance | Risco e complexidade |
| --- | --- | --- |
| **A. Corrigir a arquitetura atual** | Preservar pairing discovery, endpoint discovery, mudança automática de IP, conexão persistente e cache autenticado. Exige separar sinal confiável de mudança de rota LAN de alterações transitórias de propriedades, coordenar callback/geração/fechamento e garantir recuperação em todos os pontos entre autenticação e rodada. | Maior superfície de concorrência e testes em Android real; menor mudança conceitual nas funcionalidades. |
| **B. Restaurar seletivamente a regra antiga de falha** | Para erro de transporte recuperável, descartar sempre stream e clone, resolver endpoint quando preciso e autenticar de novo na próxima tentativa; discovery continua apenas localizando o endpoint. Preserva pairing e pode manter conexão persistente entre rodadas bem-sucedidas. | **Menor risco e complexidade como primeiro passo**, porque a regra de descarte no limite de erro já existe no baseline e na rodada atual. Ainda precisa evitar que callbacks transitórios matem indefinidamente cada conexão nova; sozinha não resolve discovery indisponível. |

## 7. Recomendação mínima e pontos prováveis de mudança

Priorizar **B como regra de recuperação**, preservando as funcionalidades atuais: todo erro de transporte recuperável deve terminar com `CONNECTION`/`ACTIVE_SOCKET` sem stream utilizável, permitir nova resolução/autenticação e manter o worker em retry. A partir dessa regra, revisar o `networkSignature` e a política de invalidação para que mudanças transitórias não destruam uma conexão útil repetidamente. Não há motivo demonstrado neste histórico para mexer no protocolo ou no pairing.

Arquivos/funções que provavelmente precisariam ser avaliados em uma etapa posterior de correção: `android/app/src/main/java/app/rowd/SyncService.kt::observeNetwork` e loop de retry de `onStartCommand`; `crates/rowd-android/src/lib.rs::Java_app_rowd_NativeBridge_networkChanged`, `sync`, `pollWake` e `clear_connection`; `crates/rowd-core/src/discovery.rs::EndpointResolver::connect`. `crates/rowd-app/src/lib.rs::discovery_responder` só entraria se a coleta de evidência apontar falha de anúncio do PC. `managed.rs` e o pairing não mostram alteração necessária para a hipótese principal.

**Evidência necessária para elevar a confiança:** log no aparelho com timestamp e valor de `networkSignature`/geração em cada callback, início/fim de `resolver.connect()`, endpoint testado, erro de TLS/auth, erro de `pollWake()`/rodada e estado `active`/`CANCELLED`; correlacionar com logs de aceite/encerramento de sessão e responder de discovery no PC. Se as gerações pararem de mudar e os retries continuarem falhando, a hipótese principal deve ser rebaixada em favor da falha de endpoint/servidor ou de ciclo de vida do serviço. Nenhum teste ou build foi executado nesta análise.
