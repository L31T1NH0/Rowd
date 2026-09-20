# Rowd

Sincronização bidirecional de uma pasta entre um PC Linux e um dispositivo Android pela rede local.

O Rowd usa Rust no núcleo e Kotlin no aplicativo Android. A conexão usa TLS com certificado fixado no convite e autenticação HMAC. Os arquivos ficam nos seus dispositivos: não há servidor intermediário nem serviço de nuvem.

> **Status:** versão 0.1.0. O fluxo Rust/PC foi validado; a compilação do Android foi validada, mas testes de uso em aparelho real ainda são necessários. Use uma pasta descartável antes de sincronizar dados importantes.

## Recursos

- Sincronização nos dois sentidos por rodadas, sem conexão permanente.
- Verificação de SHA-256 e tamanho antes de cada transferência e escrita.
- Conflitos preservados em cópias separadas, sem escolher uma versão por data.
- Convite privado com identidade da pasta, certificado TLS e credencial de acesso.
- Backups locais e recuperação após interrupções durante a escrita.
- Modo manual e sincronização automática no Android.
- CLI para operar o PC e simular o celular com outra pasta.

## Como funciona

```text
PC Linux                         Android
rowd serve  <── TLS + HMAC ──  aplicativo Rowd
     pasta compartilhada          pasta escolhida pelo usuário
```

O PC coordena cada rodada e mantém o estado-base do pareamento. O Android inicia a conexão. A cada rodada, o Rowd lê os dois manifestos, compara hashes e transfere apenas o necessário.

O pareamento vincula uma raiz Android a uma pasta do PC. Para usar outra pasta, gere um novo convite.

## Requisitos

### PC

- Linux com Rust estável.
- PC e Android na mesma rede local.
- Porta TCP `43821` liberada no firewall do PC.

### Android

- Android 8 ou superior, API 26+.
- Arquitetura ARM64 (`arm64-v8a`).
- Acesso à pasta concedido pelo seletor de documentos do Android.

## Início rápido

### 1. Prepare o PC

Compile a CLI:

```bash
cargo build --release -p rowd
```

Escolha uma pasta dedicada e gere um convite. Troque `192.168.1.20` pelo endereço do PC na rede local; `hostname -I` ajuda a encontrá-lo.

```bash
./target/release/rowd init \
  --folder "$HOME/Rowd" \
  --address 192.168.1.20:43821 \
  --invite "$HOME/rowd-convite.json"
```

Inicie o servidor e mantenha o processo aberto durante o uso:

```bash
./target/release/rowd serve --folder "$HOME/Rowd"
```

O endereço passado a `init` precisa apontar para o PC. O servidor escuta em `0.0.0.0:43821` por padrão. Não encaminhe essa porta no roteador.

### 2. Transfira o convite

Leve `rowd-convite.json` ao celular por USB ou outro canal confiável. O convite dá acesso à pasta pareada.

Não coloque o arquivo dentro da pasta sincronizada nem o publique no Git. Ao importar, compare o SHA-256 do certificado mostrado no terminal com o fingerprint exibido pelo Android.

### 3. Compile e configure o Android

Os scripts baixam JDK 17, SDK 35, NDK 27.2 e Gradle 8.9 para `.toolchain/`:

```bash
bash scripts/android-tools.sh
bash scripts/build-android.sh
```

O APK fica em:

```text
android/app/build/outputs/apk/debug/app-debug.apk
```

Instale o APK no Android ARM64 e abra o Rowd:

1. Escolha a pasta que será sincronizada.
2. Importe `rowd-convite.json`.
3. Confirme o endereço e o fingerprint do PC.
4. Toque em **Sincronizar agora**.
5. Ative **Iniciar sincronização automática** se quiser repetir as rodadas.

O modo automático inicia uma nova rodada após 10 segundos quando a anterior termina. Se a rede falhar, o intervalo aumenta até 60 segundos. O Android pode interromper o serviço para economizar bateria; abra o app para retomar.

## CLI

Use `rowd --help` para a ajuda completa.

| Comando | Uso |
| --- | --- |
| `init` | Inicializa a pasta do PC e cria um convite privado. Requer `--folder`, `--address` e `--invite`. |
| `serve` | Aguarda conexões do Android. Aceita `--listen` e `--once`; a porta padrão é `43821`. |
| `sync` | Executa uma rodada como cliente usando um convite. Serve para testes com outra pasta; `--watch` repete as tentativas. |
| `status` | Calcula e exibe em JSON o manifesto atual da pasta. |

Exemplo de teste sem aparelho:

```bash
./target/release/rowd sync \
  --folder /tmp/rowd-celular-teste \
  --invite "$HOME/rowd-convite.json"
```

Use um pareamento dedicado para esse teste. A primeira raiz cliente autenticada fica vinculada ao PC.

## Regras de sincronização

- Arquivos novos e alterações seguem nos dois sentidos.
- O Rowd recalcula SHA-256 em cada varredura; não usa data de modificação como atalho.
- O tamanho e o hash são conferidos antes de aplicar cada arquivo recebido.
- O remetente lê uma cópia temporária validada, para não enviar conteúdo diferente do manifesto.
- Se o destino mudar durante a escrita, a rodada retorna `STALE_TARGET` e tenta de novo na próxima rodada.
- Se os dois lados alterarem o mesmo arquivo, o conteúdo do PC permanece no caminho original. A versão do Android é preservada em `Rowd Conflicts/<hash-do-caminho>/<hash-do-conteúdo>/<nome-original>` nos dois dispositivos.
- A repetição do mesmo conflito é inofensiva; hashes completos evitam colisões de nomes.
- Exclusões não são propagadas. Um arquivo apagado de um lado pode voltar na próxima rodada.
- Renomear equivale a criar um caminho novo; o caminho antigo pode reaparecer.
- Pastas vazias, permissões POSIX, datas originais e links simbólicos não são sincronizados.
- Nomes incompatíveis, colisões entre maiúsculas e minúsculas, colisões entre arquivo e diretório e caminhos fora da raiz interrompem a rodada.

## Segurança e recuperação

O convite contém:

- o endereço do PC;
- os identificadores do pareamento e da pasta;
- o certificado TLS exato que o Android deve confiar;
- um segredo aleatório de 256 bits para o desafio HMAC.

O PC não abre um endpoint de cadastro. A conexão usa TLS e o protocolo autentica a raiz Android antes de sincronizar. Qualquer pessoa com o convite pode tentar acessar a pasta; trate o arquivo como uma credencial.

No PC, `.rowd/` guarda identidade, estado, trava de processo e backups. No Android, os registros e arquivos de recuperação ficam no armazenamento privado do app. Os backups não são apagados automaticamente.

Se uma escrita no Android ficar ambígua, o Rowd interrompe a sincronização e conserva as cópias `.old`, `.new` e o JSON que identifica o caminho original. Use **Exportar cópias de recuperação**, escolha a versão desejada e deixe a próxima varredura liberar a rodada.

Não desinstale o app nem limpe os dados antes de exportar esses arquivos.

O Storage Access Framework do Android não oferece compare-and-swap universal. As precondições, cópias e verificações reduzem o risco, mas não detectam toda edição externa feita entre a última leitura e a escrita. Evite editar o mesmo arquivo durante o recebimento, sobretudo em provedores de nuvem.

## Limites conhecidos

- 8 GiB por arquivo.
- 50 mil arquivos por pasta.
- 16 MiB por mensagem de controle.
- Uma transferência de arquivo por vez em cada sessão.
- Apenas Android ARM64 nesta versão.
- Sem propagação de exclusões.
- Sem garantia de atomicidade para edições externas concorrentes via SAF.
- O serviço em primeiro plano `dataSync` possui limites de execução no Android 15 ou superior.

Comece com uma pasta pequena de teste. O registro de validação separa os cenários executados dos testes que ainda dependem de um aparelho real.

## Desenvolvimento

Verifique o workspace Rust com:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Os testes de integração usam TCP/TLS real em portas de loopback temporárias e cobrem reconciliação, conflitos, hashes, recuperação, caminhos inválidos e transferências entre os processos CLI.

Para compilar o Android, os scripts usam JDK 17, Gradle 8.9, SDK 35 e NDK 27.2. O instalador solicita a aceitação das licenças do SDK.

### Estrutura

```text
crates/rowd-core/     hashes, reconciliação, protocolo, TLS e armazenamento
crates/rowd/          CLI: init, serve, sync e status
crates/rowd-android/  ponte JNI entre Rust e Android
android/              interface, serviço e acesso SAF
scripts/              preparação da toolchain e build Android
docs/                 arquitetura e validação
```

## Documentação

- [Arquitetura](docs/ARCHITECTURE.md): responsabilidades, protocolo, convergência e recuperação.
- [Validação](docs/VALIDATION.md): comandos executados, artefatos gerados e lacunas de teste.

## Licença

Este projeto usa a licença MIT, declarada no [Cargo.toml do workspace](Cargo.toml).
