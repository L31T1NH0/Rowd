# Rowd

Sincronização de vários diretórios entre **um PC Linux e um Android**, pela rede local. Rust no núcleo, Ratatui no terminal e Kotlin/SAF no Android. Sem servidor intermediário.

**V4 · 0.4.0:** a lógica desktop compartilhada vive em `rowd-app`; CLI e TUI são frontends. A administração inclui pausa global/por Share, reindexação, remapeamento explícito, ciclo completo de solicitações, resets graduais, backups criptografados, diagnóstico e recovery.

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
2. No Android, toque em **Parear por QR** e compare o fingerprint com o PC. O JSON continua disponível como alternativa.
3. Toque em **Escolher pasta para novo Share**, selecione a pasta Android, informe o nome e o modo.
4. Na TUI do PC, abra **2 Solicitações**, selecione a solicitação, pressione **a** e informe a pasta correspondente no PC com seu caminho absoluto.
5. Sincronize novamente. O mesmo fluxo vale para `/home/leite/Documents/Rowd ↔ Android/Rowd`, `/home/leite/wiki ↔ Android/Documents/blog` ou qualquer outro par de pastas sem sobreposição.
6. Um Share também pode começar no PC com **a**; depois da primeira conexão, use **Vincular pasta a Share pendente** no Android para escolher seu outro extremo.

O QR usa payload binário Base64 e blocos de meia altura. Pressione **o** na aba Dispositivo para exibi-lo dentro da TUI. O SVG privado continua disponível como fallback e contém a mesma credencial do convite.

A TUI inicia o servidor automaticamente. Mantenha o Rowd aberto nos dois dispositivos. PC e Android precisam estar na mesma rede, com TCP `43821` acessível no PC.

| Tecla | Ação global |
| --- | --- |
| 1..5 | Abrir Shares, Solicitações, Recovery, Dispositivo ou Configuração |
| Tab / Shift+Tab | Próxima aba / aba anterior |
| ↑ / ↓ | Selecionar item |
| ? | Abrir ajuda com todas as ações contextuais |
| Esc | Fechar modal |
| q | Sair |

O rodapé mostra somente ações da aba atual. Na aba Configuração, **Enter** altera o atalho selecionado; atalhos e densidade ficam em `.rowd/ui.json`, separados da configuração crítica. O painel mostra pendências, conflitos, último sincronismo, último erro persistente e progresso da rodada. Um arquivo grande ainda é transferido inteiro, sem retomada.

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
./rowd share pause ID
./rowd share resume ID
./rowd share reindex ID
./rowd share remap ID --android NovoDestino --policy compare
./rowd share sync ID
./rowd share remove ID --confirm
./rowd scan
```

No Android real, a pasta é sempre escolhida explicitamente pelo seletor SAF. `--android caminho/relativo` permanece apenas para o cliente local `device-sync`. Ao remapear, escolha `pc`, `android` ou `compare`; o Android exige uma nova seleção SAF antes da primeira rodada. Raízes PC iguais, ancestrais ou descendentes, inclusive por symlink, são recusadas. Pastas Android iguais, ancestrais ou descendentes também são recusadas.

Administração também está disponível pela CLI:

```bash
./rowd request list
./rowd request accept ID --folder /pasta/local
./rowd request reject ID
./rowd device test
./rowd device pause
./rowd device unlink --confirm
./rowd config export-profile /tmp/rowd-profile.json
./rowd config export-backup /tmp/rowd-backup.json --passphrase 'senha longa'
./rowd diagnostic --output /tmp/rowd-diagnostico.json
./rowd reset --level interface --confirm
```

Perfis não contêm credenciais. Backups completos usam PBKDF2-HMAC-SHA256 e AES-256-GCM, são criados com permissão privada e exigem senha de pelo menos oito caracteres. O relatório de diagnóstico não inclui segredo nem chave privada.

`./rowd run --listen 0.0.0.0:43821` mantém todos os Shares e o watcher em um processo. Para simular o Android sem aparelho, use uma raiz de teste exclusiva:

```bash
./rowd device-sync --folder /tmp/rowd-android --invite /tmp/rowd-convite.json --watch
```

A primeira identidade Android autenticada fica vinculada ao PC. Essa identidade pertence ao aparelho, não a uma pasta. Outro cliente é recusado.

### Solicitar um Share pelo Android

O Android não escolhe a pasta do PC diretamente. Toque em **Escolher pasta para novo Share**, escolha qualquer pasta local, informe o nome e o modo; a solicitação fica salva no telefone até ser entregue.

Na aba Solicitações da TUI, **a** aceita e **r** rejeita. Informe o caminho absoluto da pasta do PC ao aceitar. No Android, uma solicitação pendente pode ser cancelada e o histórico informa se ela foi aceita, rejeitada ou cancelada. Estados terminais convergem na próxima conexão e não reaparecem indefinidamente.

Se o PC estiver offline, a solicitação permanece pendente no Android. O envio é repetido com o mesmo ID até o PC aceitar, evitando Shares duplicados.

### Migrar V1

Pare os processos antigos e atualize PC e APK juntos. O protocolo de rede V4 rejeita versões anteriores explicitamente; o formato lógico do convite continua na versão 1, independente da versão do protocolo e dos pacotes.

```bash
./rowd migrate --folder /caminho/da/pasta-v1 --address 192.168.1.20:43821
./rowd
```

A migração reutiliza credenciais, identidade do Android, ID da pasta e estado-base. Os backups continuam na raiz original. O convite JSON já importado permanece válido.

O primeiro Share migrado conserva a antiga pasta Android como seu extremo explícito. Shares antigos que ainda dependiam de subpastas inferidas ficam pausados até que o usuário escolha sua pasta pelo botão **Vincular pasta a Share pendente**; nenhum caminho é adivinhado durante a atualização.

Os comandos `init`, `serve --folder`, `sync --folder` e `status --folder` continuam disponíveis para operar uma pasta, com o protocolo atualizado. O APK V4 usa a administração multi-Share; use `migrate` e `run` para conectar uma instalação antiga.

## Pendências, cache e eventos

Cada Share possui manifesto, base conhecida e journal JSON atômico próprios. Enquanto o outro dispositivo está offline, novas versões substituem a pendência anterior do mesmo caminho. A fila só confirma a versão cujo hash foi entregue e reconhecido. Uma queda antes do ACK mantém a pendência; repetir uma instalação é idempotente.

No Linux, inotify marca alterações e um debounce de 350 ms agrupa eventos. A varredura consulta metadados e reutiliza hashes quando dispositivo, inode, tamanho, mtime e ctime com nanossegundos permanecem iguais. Snapshots e instalações sempre conferem SHA-256. Há verificação periódica por metadados a cada minuto, scan completo periódico a cada 15 minutos e reconstrução após overflow; `scan` força todos os Shares e **x** reindexa o Share selecionado.

No Android, avisos do DocumentsProvider e mudanças administrativas acordam a próxima rodada, com fallback de 5 segundos no modo automático. A pasta SAF selecionada fica congelada durante uma rodada; um novo vínculo passa a valer na rodada seguinte. SAF não fornece metadados ou eventos universalmente confiáveis: o fallback recalcula hashes. A fila local é atualizada antes de tentar a conexão, inclusive quando o PC está offline. O Android inicia cada sessão; alterações no PC são entregues na próxima conexão do telefone.

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

Na aba Recovery da TUI, **f** percorre os filtros por Share; o painel mostra contagem e espaço total e por Share. **Enter** restaura, **k** mantém a versão atual, **e** exporta e **d** limpa manualmente um registro já resolvido. Restaurar também conserva a versão deslocada; exportar nunca substitui um arquivo existente.

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

Os testes usam sockets locais TCP/TLS e Unix; precisam de um ambiente que permita esses sockets. Consulte [Validação V4](docs/V4_VALIDATION.md) para o estado desta refatoração e os cenários que ainda exigem aparelho.

Limites: 8 GiB por arquivo, 50 mil arquivos por Share, 16 MiB por mensagem, até 256 Shares no protocolo, uma transferência por vez. Symlinks, pastas vazias, permissões e timestamps originais não são sincronizados. Não há daemon, mDNS, conexão persistente, múltiplos dispositivos, blocos ou retomada nesta versão.

## Estrutura

- `crates/rowd-core`: protocolo, modelos compartilhados, reconciliação, TLS/HMAC, journal e armazenamento.
- `crates/rowd-app`: casos de uso desktop, configuração, servidor/watcher e operações administrativas.
- `crates/rowd`: argumentos CLI, apresentação textual e TUI Ratatui.
- `crates/rowd-android`: ponte JNI, utilizando o mesmo serviço cliente Rust.
- `android`: interface, serviço e acesso SAF.

Licença MIT, declarada no [Cargo.toml](Cargo.toml).
