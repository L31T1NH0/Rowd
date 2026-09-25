# Arquitetura implementada — Rowd 0.4

## Camadas

```text
rowd (CLI + Ratatui) ──→ rowd-app ──→ rowd-core
                                      ↑
rowd-android (JNI) ───────────────────┘
```

`rowd-core` contém protocolo, TLS/HMAC, reconciliação, storage, recovery físico e modelos compartilhados. `rowd-app` expõe a façade `App` para os frontends: configuração, pareamento, Shares, solicitações, recovery, watcher/servidor, import/export, administração e diagnóstico. O binário `rowd` traduz argumentos e `UiAction`s para essa API; não abre storage nem manipula material TLS. `AppSnapshot` é uma visão derivada com `DeviceStatus`, Shares, solicitações e recovery. O QR pertence a `PairingInfo`, gerado sob demanda.

Uma futura GUI Slint deve ser outro frontend de `rowd-app`, sem depender do binário CLI nem de Ratatui.

## Responsabilidades

O PC escuta TCP e coordena uma rodada por conexão. O Android inicia a conexão,
responde aos comandos e encerra após `Done`. O serviço Android permanece ativo,
aguarda eventos ou backoff e faz auditoria completa periódica. `Sincronizar agora`
inicia o serviço parado ou acorda o serviço ativo. O PC usa watcher para marcar
paths alterados; a auditoria periódica cobre eventos perdidos.

O estado-base pertence ao PC. Como existe um coordenador fixo e só um par,
duplicar e reconciliar dois arquivos de estado acrescentaria estados de falha.
Perder uma confirmação pode provocar uma cópia de conflito conservadora; nunca
é motivo para escolher uma versão por data.

O dispositivo possui um identificador aleatório persistente, independente das
pastas. O PC o fixa após autenticar a primeira sessão. Cada Share liga uma raiz
PC a uma URI SAF escolhida explicitamente no Android e possui identidade e estado
próprios. Não existe uma raiz Android global nem destino inferido. No PC, um
arquivo de trava impede dois processos Rowd de coordenar a mesma raiz local.

## Conexão

O convite é transferido fora do canal de sincronização. Ele inclui o certificado
TLS exato a confiar e um segredo aleatório de 256 bits. O PC não precisa deixar
um endpoint de cadastro aberto.

As regras Android excluem credenciais, identidade e arquivos privados dos backups
em nuvem e das transferências automáticas entre aparelhos. Um novo celular deve
ser configurado explicitamente; as cópias de recuperação são exportadas pelo app.

```text
TLS com certificado confiado pelo convite
→ Hello(version, pair_id, device_id)
→ Challenge(nonce aleatório)
→ Proof(HMAC-SHA256)
→ Ready
→ SelectShare / Ready
→ DeltaScan / DeltaManifest ou Scan / manifesto chunked
→ Get / Blob ou Put / Accept, em janela FIFO limitada
→ Done(base_token)
```

O HMAC inclui um prefixo de domínio, nonce e identidades decodificadas de tamanho
fixo. A comparação usa a função de verificação da biblioteca HMAC. TLS permanece
responsável pela proteção do transporte e pela identidade do servidor.

Mensagens são JSON com prefixo de tamanho `u32` big-endian; blobs têm o tamanho
anunciado no cabeçalho. O receptor valida limites antes de alocar ou copiar.

O protocolo 6 inclui capacidades de desvinculação, estados explícitos de solicitação (`pending`, `accepted`, `rejected`, `cancelled`), manifesto chunked, ACKs em batch e delta para arquivos conhecidos. O QR usa um envelope binário `rowd1:` codificado em Base64 URL-safe, mas conserva endereço, certificado, segredo e todas as identidades do convite JSON.

## Administração e concorrência

Mutações comuns de configuração são atômicas e podem ocorrer durante uma rodada; a rodada atual termina com o snapshot que iniciou e a próxima recarrega a configuração. Tombstones de remoção só são limpos depois de terem sido efetivamente anunciados ao Android.

Operações que reconstroem ou substituem estado — reindex, remap, aceite/rejeição, import, reset, unlink e recovery — usam uma trava de sessão separada. Elas aguardam o ponto seguro entre rodadas sem bloquear edições comuns. Antes de mutações estruturais, o Rowd mantém até oito cópias privadas da configuração.

Configuração de dispositivo é versionada; a versão 2 migra para 3 sem recriar vínculo ou Shares. A TUI usa atalhos fixos e não produz `ui.json`. Profile e backup novos omitem `ui`; importações antigas que contêm esse campo o ignoram. Backup completo é autenticado e criptografado com PBKDF2-HMAC-SHA256 + AES-256-GCM; perfil sem segredos e diagnóstico sanitizado são formatos distintos. A API `ManagedClient` exige estado de sessão e operações de solicitação/unlink explícitas; `Store` continua separado.

## Convergência

A função pura compara `base`, PC e Android, incluindo ausência. Ela é exercitada
por todas as 64 combinações entre ausência e três hashes representativos.

`base.json` no PC é a memória autoritativa da convergência. Um `base_token` persistido
no PC e mantido apenas em memória no Android permite delta após uma rodada completa
bem-sucedida na sessão. O token é invalidado antes da próxima rodada e renovado
somente após salvar a base e enviar `Done`. O delta revalida apenas a união dos
paths dirty conhecidos; criação, remoção, rename, remap, mudança de root/ignore/
binding, token divergente e qualquer dúvida exigem full audit. Cache, dirty paths
e token são hints, nunca fonte de verdade.

Um conflito tem duas cópias finais: PC no caminho original, Android no caminho
derivado dos hashes completos do caminho e do conteúdo, mantendo o nome original
no último componente. As duas cópias Android
são confirmadas antes de substituir seu original. Se já existe uma cópia de
conflito diferente, o Rowd interrompe em vez de sobrescrevê-la.

## Escrita

O remetente prepara uma cópia temporária e confirma o hash anunciado. O receptor
valida novamente tamanho e hash e exige o estado anterior esperado. No Linux, o
destino é deslocado para um backup, seu hash é conferido e o novo arquivo é
publicado sem sobrescrever um destino que tenha aparecido durante a operação.
O inode deslocado fica preservado inclusive para escritores com descritores
antigos abertos. Essa publicação tem um breve intervalo de ausência, diferente
de um único rename substituindo o arquivo.

Transferências simples usam uma janela FIFO bloqueante de até 4 arquivos e 8 MiB
de staging por batch. `Accept` confirma cada Put em ordem antes de atualizar a
base. Get recebe blobs verificados e instala em ordem. Arquivos acima de 8 MiB,
conflitos e mudanças de direção drenam a janela e seguem serialmente.

No SAF, backups e journal são mantidos no armazenamento privado. Uma gravação
interrompida é reconhecida automaticamente apenas se o destino é exatamente a
versão antiga ou a nova. Casos ambíguos bloqueiam sync e oferecem exportação para
recuperação manual. Escritores externos não cooperativos não estão cobertos por
uma garantia de CAS: o SAF não fornece essa primitiva.

## Diferenças deliberadas do planejamento inicial

- I/O bloqueante numa única sessão substitui Tokio: não há sessões concorrentes.
- Convite privado substitui um endpoint de pareamento e um código temporário.
- Rodadas por conexão usam hints de watcher/SAF e full audit periódico.
- Só o PC persiste a base; o Android guarda pareamento e registros de recuperação.
- SHA-256 prova cada conteúdo; caches confiáveis evitam hash de paths não alterados.
- Backups são retidos, sem descarte automático baseado numa varredura posterior.
- SAF não promete atomicidade que a plataforma não disponibiliza.

São reduções de mecanismos, com os limites registrados no README. O aplicativo
continua tendo um PC, um Android, autenticação dos dois lados e tratamento
determinístico de conflitos.
