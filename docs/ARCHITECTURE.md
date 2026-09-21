# Arquitetura implementada — Rowd 0.3

## Responsabilidades

O PC escuta TCP e coordena uma rodada por conexão. O Android inicia a conexão,
responde aos comandos e encerra após `Done`. Em modo automático ele repete o
ciclo depois de 10 segundos. A reconexão periódica também encontra mudanças no
PC, sem um protocolo adicional de notificações ou um observador de arquivos.

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
→ Hello(version, pair_id, folder_id, device_id)
→ Challenge(nonce aleatório)
→ Proof(HMAC-SHA256)
→ Ready
→ Scan / Files
→ Get / Blob ou Put / Accept, um arquivo por vez
→ Done
```

O HMAC inclui um prefixo de domínio, nonce e identidades decodificadas de tamanho
fixo. A comparação usa a função de verificação da biblioteca HMAC. TLS permanece
responsável pela proteção do transporte e pela identidade do servidor.

Mensagens são JSON com prefixo de tamanho `u32` big-endian; blobs têm o tamanho
anunciado no cabeçalho. O receptor valida limites antes de alocar ou copiar.

## Convergência

A função pura compara `base`, PC e Android, incluindo ausência. Ela é exercitada
por todas as 64 combinações entre ausência e três hashes representativos.

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

No SAF, backups e journal são mantidos no armazenamento privado. Uma gravação
interrompida é reconhecida automaticamente apenas se o destino é exatamente a
versão antiga ou a nova. Casos ambíguos bloqueiam sync e oferecem exportação para
recuperação manual. Escritores externos não cooperativos não estão cobertos por
uma garantia de CAS: o SAF não fornece essa primitiva.

## Diferenças deliberadas do planejamento inicial

- I/O bloqueante numa única sessão substitui Tokio: não há sessões concorrentes.
- Convite privado substitui um endpoint de pareamento e um código temporário.
- Rodadas periódicas substituem conexão permanente, `dirty` e `notify` na V1.
- Só o PC persiste a base; o Android guarda pareamento e registros de recuperação.
- SHA-256 é recalculado; otimização por metadados ficou adiada.
- Backups são retidos, sem descarte automático baseado numa varredura posterior.
- SAF não promete atomicidade que a plataforma não disponibiliza.

São reduções de mecanismos, com os limites registrados no README. O aplicativo
continua tendo uma pasta, dois dispositivos, transferência sequencial, autenticação
dos dois lados e tratamento determinístico de conflitos.
