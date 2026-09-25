# Rowd

> **Parte 1:** apresentação do projeto
> **Parte 2:** manual operacional

---

# Parte 1 — Apresentação

**Sincronização direta de arquivos entre Linux e Android, pela rede local, sem servidor intermediário.**

Rowd conecta explicitamente pastas do PC a pastas do Android por meio de **Shares**. Cada Share tem identidade, direção, base de convergência, conflitos e recovery próprios.

```text
PC ───────────── Share ───────────── Android

~/Pictures/Inbox      ←             DCIM/Camera
~/Pictures/Selected   →             Pictures/Selected
~/Documents/Notes     ↔             Documents/Notes
```

A ideia é simples: o Rowd não tenta decidir como seus arquivos devem circular. Você define as rotas; ele executa a sincronização e preserva integridade e estado.

## Por que Rowd?

- **Sem nuvem obrigatória:** os arquivos trafegam diretamente entre os dispositivos.
- **Rede local:** PC e Android precisam apenas conseguir se alcançar pela LAN.
- **Direção por Share:** bidirecional, PC → Android ou Android → PC.
- **Vários Shares independentes:** cada par de pastas tem seu próprio estado.
- **Operação headless:** configure pela TUI ou CLI e depois rode apenas o runtime.
- **Integridade:** snapshots e instalações conferem SHA-256.
- **Conflitos conservadores:** versões concorrentes são preservadas em vez de sobrescritas silenciosamente.
- **Recovery:** escritas ambíguas mantêm cópias para recuperação manual.
- **Sem exclusão propagada:** remover um arquivo em um lado não apaga automaticamente a última versão do outro.

## Como funciona

Um Share liga uma pasta do Linux a uma pasta escolhida explicitamente no Android.

Exemplo:

```text
Share: Fotos recebidas
Android → PC

Android/DCIM/Camera
        ↓
~/Pictures/Camera
```

Outro Share pode usar a direção oposta:

```text
Share: Fotos selecionadas
PC → Android

~/Pictures/Selected
        ↓
Android/Pictures/Selected
```

Os dois podem coexistir. O comportamento pertence ao Share, não ao tipo de arquivo.

### Modos

| Modo | Fluxo |
| --- | --- |
| `bidirectional` | PC ↔ Android |
| `to_android` | PC → Android |
| `to_pc` | Android → PC |

Se os dois lados modificarem o mesmo caminho em modo bidirecional, a versão do PC permanece no caminho original e a versão Android é preservada em `Rowd Conflicts/...` nos dois lados.

## Arquitetura

```text
rowd (CLI + Ratatui) ──→ rowd-app ──→ rowd-core
                                      ↑
rowd-android (JNI) ───────────────────┘
```

- **`rowd-core`** — protocolo, modelos compartilhados, TLS/HMAC, reconciliação, recovery e armazenamento.
- **`rowd-app`** — casos de uso desktop, configuração, servidor/watcher e operações administrativas.
- **`rowd`** — argumentos CLI, apresentação textual e TUI Ratatui.
- **`rowd-android`** — ponte JNI para o mesmo núcleo Rust.
- **`android`** — interface, serviço em primeiro plano e acesso SAF.

A TUI é apenas um frontend. A lógica de aplicação vive em `rowd-app`, o que permite operar o Rowd também pela CLI e em modo headless.

## Segurança e integridade

O pareamento usa convite privado com certificado e credencial. A conexão usa TLS e autenticação HMAC.

O Rowd também aplica:

- SHA-256 em snapshots e instalações;
- validação de caminhos;
- escrita condicional para detectar destino alterado;
- estado-base persistente por Share e recovery físico;
- ACK ligado à versão efetivamente entregue;
- configuração e estado gravados de forma atômica;
- recovery quando uma escrita fica ambígua.

Perfis exportados não incluem credenciais. Backups completos são criptografados com PBKDF2-HMAC-SHA256 e AES-256-GCM.

## Eventos e funcionamento offline

No Linux, inotify marca mudanças e um debounce de 150 ms agrupa eventos. Um evento confiável consulta apenas os caminhos alterados; auditorias periódicas cobrem eventos perdidos.

No Android, eventos do DocumentsProvider acordam a próxima rodada quando disponíveis. URIs conhecidas permitem consultar um caminho; auditorias de namespace encontram mudanças não identificadas, e auditorias profundas refazem hashes quando a confiança é perdida.

Se um dispositivo estiver offline, o conteúdo permanece nas pastas e a próxima conexão reconcilia os dois lados com a base conhecida.

## Operação com ou sem interface

A TUI abre com:

```bash
./rowd
```

Depois de configurar o ambiente, o Rowd pode rodar sem interface:

```bash
./rowd run
```

Também é possível administrar Shares, dispositivo, recovery, backups e diagnóstico diretamente pela CLI.

## Estado atual

**Protocolo V8 · Rowd 0.5.0**

A versão atual suporta:

- um PC Linux;
- um Android;
- vários Shares;
- TUI Ratatui;
- CLI completa;
- operação headless;
- pausa global e por Share;
- reindexação;
- remapeamento explícito;
- solicitações de Share com estados completos;
- recovery;
- perfil sem segredos;
- backup completo criptografado;
- diagnóstico sanitizado;
- conexão TLS reutilizada entre rodadas;
- sincronização focada por Share e caminho;
- delta para arquivos novos;
- auditorias de namespace e profundas separadas.

Os arquivos publicados em [Releases](https://github.com/L31T1NH0/Rowd/releases) usam nomes específicos da versão e plataforma:

- `rowd-v0.5.0-linux-x86_64`: executável Linux;
- `rowd-v0.5.0-android-arm64.apk`: APK Android 8+ ARM64, assinado com chave debug;
- `rowd-v0.5.0-SHA256SUMS`: hashes dos dois arquivos.

## Limites atuais

- 8 GiB por arquivo;
- 50 mil arquivos por Share;
- 16 MiB por mensagem;
- até 256 Shares no protocolo;
- janela FIFO de até quatro arquivos e 8 MiB de staging por lote;
- sem retomada parcial de arquivos;
- sem sincronização de symlinks;
- sem sincronização de pastas vazias;
- sem preservação de permissões e timestamps originais;
- sem daemon nativo;
- sem mDNS;
- sem múltiplos dispositivos nesta versão;
- sem sync por blocos.

## Desenvolvimento

Requisitos Android atuais: API 26+, ARM64, JDK 17, SDK 35, NDK 27.2 e Gradle 8.9.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --offline
cargo test --workspace --offline
```

Para detalhes de implementação, consulte:

- `docs/ARCHITECTURE.md`

---

# Parte 2 — Manual operacional

## 1. Início rápido

Com Rust instalado, no PC:

```bash
./rowd
```

O script compila o projeto e abre a TUI.

Para gerar e executar o binário diretamente:

```bash
cargo build --release -p rowd
./target/release/rowd
```

A TUI inicia o servidor automaticamente.

PC e Android precisam estar na mesma rede, com TCP `43821` acessível no PC.

## 2. Pareamento

### Pela TUI

1. Abra a aba **Dispositivo**.
2. Inicie o pareamento.
3. Informe o endereço do PC, por exemplo `192.168.1.20:43821`.
4. Exiba o QR dentro da TUI.
5. No Android, toque em **Parear por QR**.
6. Compare o fingerprint mostrado nos dois lados antes de confirmar.

O JSON de convite continua disponível como alternativa.

O QR usa payload binário Base64 e blocos de meia altura. O SVG privado permanece disponível como fallback e contém a mesma credencial do convite.

### Pela CLI

```bash
./rowd pair --address 192.168.1.20:43821 --invite /tmp/rowd-convite.txt
```

A primeira identidade Android autenticada fica vinculada ao PC. Essa identidade pertence ao aparelho, não a uma pasta. Outro cliente é recusado.

## 3. Conceito de Share

Cada Share liga explicitamente:

```text
uma pasta Linux
↕ / → / ←
uma pasta Android
```

Cada Share possui `share_id` permanente, nome, pasta PC, pasta Android, modo, base de convergência, conflitos e recovery próprios.

Shares não podem ter raízes sobrepostas.

No Android real, a pasta é sempre escolhida pelo seletor SAF.

## 4. Modos de sincronização

### `bidirectional`

```text
PC ↔ Android
```

Mudanças seguem nos dois sentidos.

Se os dois lados editarem o mesmo caminho antes de convergir, a versão do PC permanece no caminho original e a versão Android é preservada em:

```text
Rowd Conflicts/<hash-do-caminho>/<hash-do-conteúdo>/<nome>
```

nos dois lados.

### `to_android`

```text
PC → Android
```

Somente mudanças do PC são propagadas. Uma alteração Android que exigiria envio reverso ou sobrescrita conflitante permanece no lugar e é tratada como conflito.

### `to_pc`

```text
Android → PC
```

Somente mudanças Android são propagadas. Alterações concorrentes no PC recebem a mesma proteção de conflito.

## 5. Criar um Share pelo Android

1. No Android, toque em **Escolher pasta para novo Share**.
2. Escolha a pasta local.
3. Informe o nome.
4. Escolha o modo.
5. A solicitação fica persistida no telefone até ser entregue ao PC.
6. Na TUI, abra **Solicitações**.
7. Aceite a solicitação.
8. Informe o caminho absoluto da pasta correspondente no PC.

Se o PC estiver offline, a solicitação permanece pendente no Android e é reenviada com o mesmo ID.

Estados possíveis:

```text
pending
accepted
rejected
cancelled
```

O cancelamento e a decisão do PC são confirmados na próxima conexão.

## 6. Criar um Share pelo PC

Pela TUI, use a ação de adicionar Share. Depois da primeira conexão, no Android use **Vincular pasta a Share pendente** para escolher o outro extremo.

Pela CLI:

```bash
./rowd share add   --name Projetos   --folder "$HOME/Projects"
```

Exemplo Android → PC:

```bash
./rowd share add   --name Fotos   --folder "$HOME/Pictures"   --mode to_pc
```

`share add` imprime o ID permanente do Share.

## 7. TUI

A TUI possui quatro abas:

```text
1 Shares
2 Solicitações
3 Recovery
4 Dispositivo
```

Atalhos globais padrão:

| Tecla | Ação |
| --- | --- |
| `1..4` | Abrir uma aba |
| `Tab` | Próxima aba |
| `Shift+Tab` | Aba anterior |
| `↑ / ↓` | Selecionar item |
| `?` | Ajuda |
| `Esc` | Fechar modal |
| `q` | Sair |

O rodapé mostra apenas ações válidas na aba atual.

Os atalhos são fixos e contextuais à aba atual.

## 8. CLI de Shares

```bash
./rowd shares
./rowd share edit ID --name Trabalho --mode to_android
./rowd share pause ID
./rowd share resume ID
./rowd share reindex ID
./rowd share remap ID --policy compare
./rowd share sync ID
./rowd share remove ID --confirm
./rowd scan
```

Políticas de remap:

```text
pc
android
compare
```

No Android real, um remap exige nova seleção SAF antes da primeira rodada. O PC incrementa a revisão do vínculo.

Remover um Share não apaga automaticamente conteúdo sincronizado ou recovery.

## 9. Solicitações pela CLI

```bash
./rowd request list
./rowd request accept ID --folder /pasta/local
./rowd request reject ID
```

No Android, uma solicitação pendente também pode ser cancelada.

## 10. Pausa global

```bash
./rowd device pause
./rowd device resume
```

A pausa global não remove Shares nem desfaz o pareamento.

## 11. Teste de conexão

```bash
./rowd device test
```

O diagnóstico verifica etapas da conexão e mostra quantos Shares estão disponíveis.

## 12. Desvincular dispositivo

```bash
./rowd device unlink --confirm
./rowd device revoke --confirm
```

A operação revoga o vínculo atual. Arquivos sincronizados não são apagados automaticamente.

## 13. Uso headless

Depois da configuração, a TUI não é necessária.

```bash
./rowd run
```

Endereço explícito:

```bash
./rowd run --listen 0.0.0.0:43821
```

Esse processo mantém servidor, Shares, watcher e aplicação das mudanças de configuração entre rodadas.

## 14. Configuração persistente

Por padrão:

```text
~/.local/share/rowd/.rowd/
```

É possível escolher outro local com:

```bash
--home DIRETORIO
```

ou:

```bash
ROWD_HOME=/outro/local
```

Esse diretório contém dados sensíveis, incluindo certificado, chave privada, credencial e vínculo com o Android.

## 15. Exportar perfil

```bash
./rowd config export-profile /tmp/rowd-profile.json
```

O perfil não contém credenciais.

## 16. Backup completo

Exportar:

```bash
./rowd config export-backup   /tmp/rowd-backup.json   --passphrase 'senha longa'
```

Importar:

```bash
./rowd config import-backup   /tmp/rowd-backup.json   --passphrase 'senha longa'
```

Backups completos usam PBKDF2-HMAC-SHA256 e AES-256-GCM, são privados e exigem senha de pelo menos oito caracteres.

## 17. Diagnóstico

```bash
./rowd diagnostic --output /tmp/rowd-diagnostico.json
```

O relatório não inclui segredo nem chave privada.

## 18. Reset

Exemplo:

```bash
./rowd reset --level initial --confirm
```

Os resets são graduais. Use o nível correspondente ao que precisa ser redefinido.

## 19. `.rowdignore`

Exemplo:

```text
# Comentários e linhas vazias são aceitos
node_modules/
target/
.git/
*.tmp
docs/private/
```

Regras:

- padrão sem `/` corresponde a nomes em qualquer nível;
- padrão com `/` é relativo à raiz;
- `*` é o único curinga;
- não existe negação;
- não é uma implementação completa de `.gitignore`;
- `.rowd/` e `.rowdignore` nunca são transferidos.

A política de ignore do PC é enviada ao Android. A cópia Android de `.rowdignore` não define outra política.

## 20. Exclusões e renomes

Exclusões **não são propagadas**.

Um arquivo removido em um lado pode reaparecer a partir da última versão remota.

Renomear equivale a remover o caminho antigo e criar um novo.

## 21. Base, eventos e ACK

Cada Share mantém `base.json` como memória de convergência. Cache, caminhos sujos e token de sessão são apenas pistas para acelerar a próxima rodada.

Enquanto o outro dispositivo está offline, arquivos permanecem no seu lugar. A rodada seguinte compara o estado físico com a base.

O ACK só confirma a versão cujo hash foi entregue e reconhecido.

Se a conexão cair antes do ACK, a próxima rodada reavalia o caminho; instalações e recovery preservam a segurança do replay.

## 22. Watcher Linux

No Linux:

- inotify marca alterações;
- debounce de 150 ms agrupa eventos;
- metadados permitem reutilizar hashes;
- snapshots e instalações conferem SHA-256;
- caminhos confiáveis usam delta sem enumerar todo o Share;
- criação de arquivo também pode usar delta;
- há auditoria de namespace periódica e auditoria profunda após perda de confiança;
- overflow força reconstrução.

## 23. Android e SAF

No Android:

- avisos do DocumentsProvider podem acordar a próxima rodada;
- mudanças administrativas também acordam o serviço;
- existe fallback automático quando eventos não são confiáveis;
- URI ou diretório conhecido podem limitar a consulta aos caminhos afetados;
- auditoria de namespace reutiliza hashes compatíveis do cache;
- auditoria profunda recalcula hashes após perda de confiança ou periodicamente;
- a pasta SAF usada em uma rodada fica congelada até ela terminar;
- novo vínculo entra em vigor na rodada seguinte.

O Android inicia as rodadas e mantém a conexão TLS entre elas enquanto o serviço está ativo.

SAF não oferece compare-and-swap universal; uma edição externa durante a gravação pode exigir recovery manual.

## 24. Recovery

Na TUI, a aba **Recovery** permite filtrar por Share, ver quantidade e espaço usado, restaurar, manter versão atual, exportar e limpar registros resolvidos.

CLI:

```bash
./rowd recovery
./rowd recovery --share SHARE_ID --id ID --action restore
./rowd recovery --share SHARE_ID --id ID --action export --output /tmp/versao-recuperada
```

Restaurar conserva a versão deslocada. Exportar nunca substitui um arquivo existente.

No Android, use **Revisar versões recuperáveis** e **Exportar cópias de recuperação**.

Não limpe os dados do aplicativo antes de exportar recovery necessário.

## 25. Migração de instalação antiga

Pare processos antigos e atualize PC e APK juntos.

```bash
./rowd migrate   --folder /caminho/da/pasta-v1   --address 192.168.1.20:43821

./rowd
```

A migração reutiliza credenciais, identidade Android, ID da pasta e estado-base. O protocolo de rede V8 exige atualizar o PC e o APK juntos.

Shares antigos que dependiam de destino Android inferido ficam pausados até uma pasta explícita ser escolhida.

## 26. Testes locais

```bash
cargo test -p rowd --test v2 --offline
```

O simulador de filesystem fica restrito aos testes de desenvolvimento.

## 27. Desenvolvimento Android

Requisitos atuais:

- Android 8 / API 26+;
- ARM64;
- JDK 17;
- SDK 35;
- NDK 27.2;
- Gradle 8.9.

```bash
bash scripts/android-tools.sh
bash scripts/build-android.sh
```

APK:

```text
android/app/build/outputs/apk/debug/app-debug.apk
```

## 28. Testes do workspace

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --offline
cargo test --workspace --offline
```

Os testes TCP/TLS e Unix precisam de ambiente que permita sockets locais.

Consulte `docs/ARCHITECTURE.md` para a arquitetura atual.

## 29. Limites atuais

- 8 GiB por arquivo;
- 50 mil arquivos por Share;
- 16 MiB por mensagem;
- até 256 Shares no protocolo;
- janela FIFO de até quatro arquivos e 8 MiB de staging por lote.

Não sincroniza symlinks, pastas vazias, permissões originais ou timestamps originais.

Ainda não possui daemon nativo, mDNS, múltiplos dispositivos, sync por blocos ou retomada parcial.

## 30. Estrutura do projeto

```text
crates/rowd-core
→ protocolo, modelos compartilhados, reconciliação,
  TLS/HMAC, recovery e armazenamento

crates/rowd-app
→ casos de uso desktop, configuração, servidor/watcher
  e operações administrativas

crates/rowd
→ CLI, saída textual e TUI Ratatui

crates/rowd-android
→ ponte JNI usando o mesmo núcleo Rust

android
→ interface, serviço e acesso SAF
```

Para detalhes internos, consulte:

- `docs/ARCHITECTURE.md`

---

## Licença

MIT.
