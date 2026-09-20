# Validação da versão 0.1.0

Executado no notebook Linux em 20/09/2026.

## Rust e PC

- `cargo fmt --all -- --check`: código formatado.
- `cargo clippy --workspace --all-targets -- -D warnings`: sem avisos.
- `cargo test --workspace`: 14 testes aprovados (11 de núcleo, 3 de integração).
- `cargo build --release -p rowd`: executável Linux gerado.
- `rowd --help`: comandos disponíveis e executável funcional.

Os testes cobrem as 64 combinações da reconciliação, caminhos inválidos,
links simbólicos, conflitos convergentes, repetição sem novas transferências,
edições após o manifesto, tamanho/hash incorretos, transferência truncada,
restauração de uma troca interrompida, identidade de pasta/pareamento, nonce e
HMAC inválidos. Os testes de integração usam TCP/TLS real em loopback, arquivos
vazios, Unicode e mais de 1 MiB, além dos processos CLI `init`, `serve` e `sync`.

O ambiente isolado bloqueou portas locais e uma operação usada na publicação de
arquivos. A suíte completa foi executada com essas operações permitidas; os
testes não foram desativados nem substituídos por mocks de TLS.

## Android

- Biblioteca Rust compilada para `aarch64-linux-android`.
- Classes Kotlin e interface XML compiladas com SDK 35 / JDK 17.
- Assinatura JNI conferida entre a classe compilada e o símbolo exportado.
- Biblioteca nativa com segmentos ELF alinhados a 16 KiB.
- APK debug gerado para `app.rowd`, versão 0.1.0, mínimo Android 8 (API 26).
- Verificação de assinatura APK v2 aprovada.
- Verificação `zipalign -c -P 16 4` aprovada.
- `assembleDebug lintDebug`: concluído; 0 erros e 23 avisos. Os avisos restantes
  tratam de textos não extraídos para tradução, atributos aplicáveis apenas a
  versões Android recentes, versões novas de dependências e suporte opcional
  a ChromeOS. O relatório completo está em
  `android/app/build/reports/lint-results-debug.html`.

SHA-256 dos artefatos desta compilação:

```text
e9a3e8c3b5a5f5f6b56430dc1e30d9e657c625e9177401d8089ebb5253f0a1c9  app-debug.apk
420b3ff06c2818f877abc1e741b36cb79ab93606ca650c07da8d0ee9aec80f1d  rowd (Linux)
```

## Ainda depende de um aparelho

O ADB não encontrou dispositivos conectados. Não foram executados testes de
interface, serviço em segundo plano, bateria, encerramento forçado ou integração
com um provedor SAF real. Compilar o APK e conferir a JNI não substitui esses
testes.

Antes de usar uma pasta importante, instalar o APK em ARM64 e testar numa pasta
descartável: envio em ambos os sentidos, alteração simultânea, modo automático,
Wi-Fi desligado no meio do envio e exportação/recuperação de backups. No SAF,
edições externas no intervalo da substituição não têm garantia de atomicidade.
