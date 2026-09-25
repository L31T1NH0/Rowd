# Rowd

Sincronização de vários diretórios entre **um PC Linux e um Android**, pela rede local. Rust no núcleo, Ratatui no terminal e Kotlin/SAF no Android. Sem servidor intermediário.

**0.5.0:** a sessão TCP/TLS permanece conectada entre rodadas e sincroniza os Shares acordados pelo watcher do PC ou pelo ContentObserver do Android. A lógica desktop compartilhada vive em `rowd-app`; CLI e TUI são frontends. A administração inclui pausa global/por Share, reindexação, remapeamento explícito, solicitações de Share, resets graduais, backups criptografados, diagnóstico e recovery.

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
| 1..4 | Abrir Shares, Solicitações, Recovery ou Dispositivo |
| Tab / Shift+Tab | Próxima aba / aba anterior |
| ↑ / ↓ | Selecionar item |
| ? | Abrir ajuda com todas as ações contextuais |
| Esc | Fechar modal |
| q | Sair |

O rodapé mostra os atalhos fixos da aba atual. O painel mostra pendências, conflitos, último sincronismo, último erro persistente e progresso da rodada. Um arquivo grande ainda é transferido inteiro, sem retomada.

## Configuração e CLI

A configuração fica em `~/.local/share/rowd/.rowd/`. Use `--home DIRETORIO` ou `ROWD_HOME` para escolher outro lugar. Guarde esse diretório: contém certificado, chave privada, credencial e vínculo com o Android.

```bash
./rowd pair --address 192.168.1.20:43821 --invite /tmp/rowd-convite.txt
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
./rowd share remap ID --policy compare
./rowd share sync ID
./rowd share remove ID --confirm
./rowd scan
```

No Android real, a pasta é sempre escolhida explicitamente pelo seletor SAF. Ao remapear, escolha `pc`, `android` ou `compare`; o PC incrementa a revisão do vínculo e o Android exige uma nova seleção SAF antes da primeira rodada. Raízes PC iguais, ancestrais ou descendentes, inclusive por symlink, são recusadas. Pastas Android iguais, ancestrais ou descendentes também são recusadas.

Administração também está disponível pela CLI:

```bash
./rowd request list
./rowd request accept ID --folder /pasta/local
./rowd request reject ID
./rowd device test
./rowd device pause
./rowd device unlink --confirm
./rowd device revoke --confirm  # aparelho perdido: revogação imediata, sem confirmação Android
./rowd config export-profile /tmp/rowd-profile.json
./rowd config export-backup /tmp/rowd-backup.json --passphrase 'senha longa'
./rowd diagnostic --output /tmp/rowd-diagnostico.json
./rowd reset --level initial --confirm
```

Perfis não contêm credenciais. Backups completos usam PBKDF2-HMAC-SHA256 e AES-256-GCM, são criados com permissão privada e exigem senha de pelo menos oito caracteres. O relatório de diagnóstico não inclui segredo nem chave privada.

`./rowd run --listen 0.0.0.0:43821` mantém todos os Shares e o watcher em um processo. O simulador filesystem fica restrito aos testes de desenvolvimento:

```bash
cargo test -p rowd --test v2
```

A primeira identidade Android autenticada fica vinculada ao PC. Essa identidade pertence ao aparelho, não a uma pasta. Outro cliente é recusado.

### Solicitar um Share pelo Android

O Android não escolhe a pasta do PC diretamente. Toque em **Escolher pasta para novo Share**, escolha qualquer pasta local, informe o nome e o modo; a solicitação fica salva no telefone até ser entregue.

Na aba Solicitações da TUI, **a** aceita e **r** rejeita. Informe o caminho absoluto da pasta do PC ao aceitar. No Android, uma solicitação pendente pode ser cancelada; o cancelamento e a decisão do PC são confirmados na próxima conexão. O aplicativo não mantém um histórico de decisões como fonte de verdade.

Se o PC estiver offline, a solicitação permanece pendente no Android. O envio é repetido com o mesmo ID até o PC aceitar, evitando Shares duplicados.

### Migrar V1

Pare os processos antigos e atualize PC e APK juntos. O protocolo de rede V6 rejeita versões anteriores explicitamente. O convite externo é `rowd1:` versão 2; convites JSON V1 já armazenados são convertidos pelo decoder Rust durante a migração.

```bash
./rowd migrate --folder /caminho/da/pasta-v1 --address 192.168.1.20:43821
./rowd
```

A migração reutiliza credenciais, identidade do Android, ID da pasta como Share e estado-base. Os backups continuam na raiz original. Um convite JSON V1 já importado é aceito apenas como entrada de migração; o Android o regrava em formato `rowd1:`.

O primeiro Share migrado conserva a antiga pasta Android como seu extremo explícito. Shares antigos que ainda dependiam de subpastas inferidas ficam pausados até que o usuário escolha sua pasta pelo botão **Vincular pasta a Share pendente**; nenhum caminho é adivinhado durante a atualização.

Os comandos V1 de runtime foram removidos. Use `migrate` e `run` para conectar uma instalação antiga.

## Pendências, cache e eventos

Cada Share mantém `base.json` como memória de convergência. O cache e os hints aceleram a comparação; snapshots e instalações verificam SHA-256 antes de transmitir ou publicar conteúdo, e auditorias periódicas revalidam a árvore inteira. Uma queda antes do ACK é resolvida pelo replay idempotente da instalação e pela comparação com a base. O antigo `journal.json` de sync é arquivado quando encontrado. Os registros de recovery físico permanecem separados.

No Linux, inotify marca alterações e um debounce de 350 ms agrupa eventos. Enquanto o telefone está offline, esses eventos gravam apenas um hint durável dos caminhos afetados, sem antecipar scan/hash; a conexão seguinte revalida os paths indicados. Após uma rodada completa bem-sucedida na sessão, peers com o mesmo token de base podem trocar apenas os arquivos conhecidos alterados. O delta não monta o manifesto completo. Novo arquivo, remoção, rename, mudança de política, restart ou perda de confiança forçam auditoria completa. Snapshots e instalações sempre conferem SHA-256; `scan` força todos os Shares e **x** reindexa o Share selecionado.

No Android, avisos do DocumentsProvider antecipam uma rodada focada no Share. Quando a URI identifica um arquivo conhecido, o serviço reabre e rehasha apenas esse path; no delta, só esse path entra na resposta. Avisos sem path identificável forçam scan completo **daquele Share**. Mudanças de vínculo/ignore invalidam o cache, e uma auditoria automática a cada minuto percorre todos os Shares; ela também detecta alterações do PC. A pasta SAF selecionada fica congelada durante uma rodada; um novo vínculo passa a valer na rodada seguinte. SAF não fornece eventos universalmente confiáveis: a auditoria completa continua sendo o fallback.

O benchmark local de uma rodada warm pode ser executado com `ROWD_BENCH_FILES=50000 cargo test -p rowd-core large_share_one_small_change_baseline -- --ignored --nocapture`. Ele usa dois stores de teste e socket Unix, não mede SAF físico. Após uma rodada full inicial, 50 mil arquivos iguais e uma alteração de 4 KiB produziram 2 enumerações, 1 entrada de delta, 1 path reconciliado, 0 ACKs e 0 full scans na rodada medida. Os números antes/depois estão em [métricas da implementação](plan/ROWD_IMPLEMENTACAO_METRICAS.md).

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

Padrões sem `/` correspondem a nomes em qualquer nível. Padrões com `/` são relativos à raiz. `*` é o único curinga; não há negação nem implementação completa de `.gitignore`. A política efetiva do PC é enviada ao Android; a cópia Android de `.rowdignore` não define outra política. Ignorados ficam fora do hash e das pendências. `.rowd/` e `.rowdignore` não são transferidos.

## Modos e preservação

- `bidirectional`: mudanças seguem nos dois sentidos. Se os dois lados editaram, o original PC fica no caminho e a versão Android é preservada em `Rowd Conflicts/<hash-do-caminho>/<hash-do-conteúdo>/<nome>`, nos dois lados.
- `to_android`: só PC → Android. Uma alteração Android que exigiria envio reverso ou sobrescrita conflitante permanece no lugar e aparece como conflito.
- `to_pc`: só Android → PC, com a mesma preservação para alterações PC.

Exclusões **não são propagadas**. Um arquivo removido pode voltar na próxima rodada. Renomear um arquivo equivale a remover um caminho e criar outro. Nenhuma dessas operações apaga a última versão remota.

## Recovery

Na aba Recovery da TUI, **f** percorre os filtros por Share; o painel mostra contagem e espaço total e por Share. **Enter** restaura, **k** mantém a versão atual, **e** exporta e **d** limpa manualmente um registro já resolvido. Restaurar também conserva a versão deslocada; exportar nunca substitui um arquivo existente.

```bash
./rowd recovery
./rowd recovery --share SHARE_ID --id ID --action restore
./rowd recovery --share SHARE_ID --id ID --action export --output /tmp/versao-recuperada
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

Limites: 8 GiB por arquivo, 50 mil arquivos por Share, 16 MiB por mensagem, até 256 Shares no protocolo, janela FIFO de até 4 arquivos e 8 MiB de staging por batch. Arquivos maiores seguem serialmente. Symlinks, pastas vazias, permissões e timestamps originais não são sincronizados. Não há daemon, mDNS, conexão persistente, múltiplos dispositivos, blocos ou retomada nesta versão.

## Estrutura

- `crates/rowd-core`: protocolo, modelos compartilhados, reconciliação, TLS/HMAC, recovery físico e armazenamento.
- `crates/rowd-app`: casos de uso desktop, configuração, servidor/watcher e operações administrativas.
- `crates/rowd`: argumentos CLI, apresentação textual e TUI Ratatui.
- `crates/rowd-android`: ponte JNI, utilizando o mesmo serviço cliente Rust.
- `android`: interface, serviço e acesso SAF.

Licença MIT, declarada no [Cargo.toml](Cargo.toml).
