# Rowd

Sincronização de vários diretórios entre **um PC Linux e um Android**, pela rede local. Rust no núcleo, Ratatui no terminal e Kotlin/SAF no Android. Sem servidor intermediário.

**V2 · 0.2.1:** configuração persistente, Shares, solicitações de Share iniciadas no Android, pareamento por QR/JSON, pendências com ACK, watcher Linux, modos por Share, recuperação acessível e ícone mobile próprio. A validação em aparelho real de SAF, câmera e bateria ainda está pendente; veja [Validação V2](docs/V2_VALIDATION.md).

## Começar

Com Rust instalado, abra no PC:

```bash
./rowd
```

O script compila e abre a TUI. Para um binário independente:

```bash
cargo build --release -p rowd
./target/release/rowd
```

1. Pressione **p** e informe o endereço do PC, por exemplo `192.168.1.20:43821`.
2. No Android, escolha ou crie a raiz `Rowd` pelo seletor de documentos, uma única vez.
3. Toque em **Parear por QR**. Compare o fingerprint com o PC antes de conectar. O JSON continua disponível como alternativa.
4. No PC, pressione **a** e preencha `Projetos | /home/voce/Projects | bidirectional`.
5. Ative a sincronização automática no Android. O PC cria a definição e o Android cria `Rowd/Projetos` na próxima conexão. Repita **a** para outros Shares.
6. Para iniciar um Share pelo Android, toque em **Solicitar Share**, escolha a pasta do celular (por exemplo, `DCIM`), informe o nome e o modo. Na TUI do PC, pressione **c**, informe a pasta local escolhida com `pwd` (por exemplo, `Pictures`) e confirme. O Share será criado no PC e enviado ao Android na próxima rodada.

O QR completo pode exigir um terminal maior. A tela informa o tamanho necessário; **o** abre sua imagem SVG privada. Feche a imagem depois de parear: ela contém a mesma credencial do convite.

A TUI inicia o servidor automaticamente. Mantenha o Rowd aberto nos dois dispositivos. PC e Android precisam estar na mesma rede, com TCP `43821` acessível no PC.

| Tecla | Ação |
| --- | --- |
| ↑ / ↓ | Selecionar Share |
| p | Parear / atualizar endereço do convite |
| a | Adicionar Share |
| n / c | Alternar solicitação Android / aceitar a pasta escolhida |
| e | Editar nome, raiz PC e modo |
| d | Desvincular após digitar `REMOVER`; conserva arquivos e recovery |
| s | Atualizar pendências para a próxima conexão do Android |
| v | Verificar conteúdo com scan completo |
| r | Listar versões e manter, restaurar ou exportar recovery |
| q | Sair |

O painel mostra pendências, caminhos com conflito, último sincronismo e progresso por caminhos na rodada. Um arquivo grande ainda é transferido inteiro, sem retomada.

## Configuração e CLI

A configuração fica em `~/.local/share/rowd/.rowd/`. Use `--home DIRETORIO` ou `ROWD_HOME` para escolher outro lugar. Guarde esse diretório: contém certificado, chave privada, credencial e vínculo com o Android.

```bash
./rowd pair --address 192.168.1.20:43821 --invite /tmp/rowd-convite.json
./rowd share add --name Projetos --folder "$HOME/Projects"
./rowd share add --name Fotos --folder "$HOME/Pictures" --mode to_pc
./rowd shares
./rowd run
```

`share add` imprime o ID permanente. Renomear conserva esse ID e o destino Android:

```bash
./rowd share edit ID --name Trabalho --mode to_android
./rowd share remove ID --confirm
./rowd scan
```

`--android caminho/relativo` em `share add` permite outro destino **dentro da raiz Android autorizada**. O nome visual pode mudar; o destino Android existente permanece fixo. Raízes PC iguais, ancestrais ou descendentes, inclusive por symlink, são recusadas. Diretórios internos `.rowd` não podem virar Shares.

`./rowd run --listen 0.0.0.0:43821` mantém todos os Shares e o watcher em um processo. Para simular o Android sem aparelho, use uma raiz de teste exclusiva:

```bash
./rowd device-sync --folder /tmp/rowd-android --invite /tmp/rowd-convite.json --watch
```

A primeira identidade Android autenticada fica vinculada ao PC. Outro cliente com raiz/identidade diferente é recusado.

### Solicitar um Share pelo Android

O Android não escolhe a pasta do PC diretamente. Toque em **Solicitar Share**, escolha a pasta local do celular, informe o nome e escolha o modo; a solicitação fica salva no telefone até ser entregue. Essa pasta pode estar fora da raiz Rowd vinculada.

Na TUI do PC, `n` alterna entre solicitações pendentes e `c` aceita a selecionada. Informe o caminho absoluto da pasta do PC, de preferência copiando o resultado de `pwd`, e confirme. O PC valida a pasta e cria o Share; na próxima rodada, o Android recebe a configuração e vincula o Share à pasta escolhida no celular.

Se o PC estiver offline, a solicitação permanece pendente no Android. O envio é repetido com o mesmo ID até o PC aceitar, evitando Shares duplicados.

### Migrar V1

Pare os processos antigos e atualize PC e APK juntos. O protocolo de rede V2 rejeita a V1 explicitamente; o formato do convite continua na versão 1, independente da versão do protocolo e dos pacotes.

```bash
./rowd migrate --folder /caminho/da/pasta-v1 --address 192.168.1.20:43821
./rowd
```

A migração reutiliza credenciais, identidade do Android, ID da pasta e estado-base. Os backups continuam na raiz original. O convite JSON já importado permanece válido.

O primeiro Share migrado mantém a raiz Android original. Novos Shares usam subpastas exclusivas dessa raiz e são excluídos do Share legado. Um destino que já contenha arquivos do Share legado é recusado. A exclusão dessas subpastas permanece após desvinculá-las, para não misturar os estados.

Os comandos `init`, `serve --folder`, `sync --folder` e `status --folder` continuam disponíveis para operar uma pasta, com o protocolo atualizado. O APK V2 usa a administração multi-Share; use `migrate` e `run` para conectar uma instalação antiga.

## Pendências, cache e eventos

Cada Share possui manifesto, base conhecida e journal JSON atômico próprios. Enquanto o outro dispositivo está offline, novas versões substituem a pendência anterior do mesmo caminho. A fila só confirma a versão cujo hash foi entregue e reconhecido. Uma queda antes do ACK mantém a pendência; repetir uma instalação é idempotente.

No Linux, inotify marca alterações e um debounce de 350 ms agrupa eventos. A varredura consulta metadados e reutiliza hashes quando dispositivo, inode, tamanho, mtime e ctime com nanossegundos permanecem iguais. Snapshots e instalações sempre conferem SHA-256. Há verificação periódica por metadados a cada minuto, scan completo periódico a cada 15 minutos e reconstrução após overflow; `scan`/tecla **v** força a leitura de conteúdo.

No Android, avisos do DocumentsProvider antecipam a próxima rodada, com fallback de 5 segundos no modo automático. SAF não fornece metadados ou eventos universalmente confiáveis: o fallback recalcula hashes. A fila local é atualizada antes de tentar a conexão, inclusive quando o PC está offline. O Android inicia cada sessão; alterações no PC são entregues na próxima conexão do telefone.

### `.rowdignore`

Crie por Share:

```text
# Comentários e linhas vazias são aceitos
node_modules/
target/
.git/
*.tmp
docs/private/
```

Padrões sem `/` correspondem a nomes em qualquer nível. Padrões com `/` são relativos à raiz. `*` é o único curinga; não há negação nem implementação completa de `.gitignore`. A configuração de ignore do PC é enviada ao Android; regras locais Android também são respeitadas. Ignorados ficam fora do hash e das pendências. `.rowd/` e `.rowdignore` não são transferidos.

## Modos e preservação

- `bidirectional`: mudanças seguem nos dois sentidos. Se os dois lados editaram, o original PC fica no caminho e a versão Android é preservada em `Rowd Conflicts/<hash-do-caminho>/<hash-do-conteúdo>/<nome>`, nos dois lados.
- `to_android`: só PC → Android. Uma alteração Android que exigiria envio reverso ou sobrescrita conflitante permanece no lugar e aparece como conflito.
- `to_pc`: só Android → PC, com a mesma preservação para alterações PC.

Exclusões **não são propagadas**. Um arquivo removido pode voltar na próxima rodada. Renomear um arquivo equivale a remover um caminho e criar outro. Nenhuma dessas operações apaga a última versão remota.

## Recovery

Na TUI, **r** lista os registros do Share. As ações são `keep`, `restore` e `export`. Restaurar também conserva a versão deslocada; exportar nunca substitui um arquivo existente.

```bash
./rowd recovery --folder /pasta/do/share
./rowd recovery --folder /pasta/do/share --id ID --action restore
./rowd recovery --folder /pasta/do/share --id ID --action export --output /tmp/versao-recuperada
```

No Android, **Revisar versões recuperáveis** permite manter a atual ou restaurar a anterior; **Exportar cópias de recuperação** conserva os registros e arquivos `.old`/`.new` fora do armazenamento privado. Um resultado de escrita ambíguo bloqueia novas rodadas até a escolha. Não limpe os dados do aplicativo antes de exportar.

SAF não oferece compare-and-swap universal. Precondições e backups preservam versões, mas uma edição externa durante a gravação ainda pode exigir recuperação manual.

## Android e desenvolvimento

Android 8/API 26+, ARM64. Os scripts utilizam JDK 17, SDK 35, NDK 27.2 e Gradle 8.9:

```bash
bash scripts/android-tools.sh
bash scripts/build-android.sh
```

APK: `android/app/build/outputs/apk/debug/app-debug.apk`.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Os testes usam sockets locais TCP/TLS e Unix; precisam de um ambiente que permita esses sockets. Consulte [Validação V2](docs/V2_VALIDATION.md) para os cenários executados e os que ainda exigem aparelho.

Limites: 8 GiB por arquivo, 50 mil arquivos por Share, 16 MiB por mensagem, até 256 Shares no protocolo, uma transferência por vez. Symlinks, pastas vazias, permissões e timestamps originais não são sincronizados. Não há daemon, mDNS, conexão persistente, múltiplos dispositivos, blocos ou retomada nesta versão.

## Estrutura

- `crates/rowd-core`: configuração, estado, reconciliação, TLS/HMAC, protocolo e armazenamento.
- `crates/rowd`: CLI, TUI e servidor com watcher.
- `crates/rowd-android`: ponte JNI, utilizando o mesmo serviço cliente Rust.
- `android`: interface, serviço e acesso SAF.

Licença MIT, declarada no [Cargo.toml](Cargo.toml).
