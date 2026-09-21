# Validação da implementação V4

Registro de 21/09/2026. A implementação foi revisada estaticamente e, na etapa de publicação, compilada para Linux x86_64 e Android ARM64.

## Cobertura implementada

- `rowd-app` separa casos de uso desktop de CLI/TUI e de `rowd-core`.
- TUI com cinco abas, master/detail responsivo em três larguras, modal central, ajuda e rodapé contextual.
- `UiAction` e `KeyMap` configurável persistido separadamente.
- QR denso inline e payload binário `rowd1:` aceito pelo Android, preservando credenciais.
- Pausa global e por Share; sincronização direcionada; reindex e remap com política explícita.
- Solicitações com estados pending/accepted/rejected/cancelled e histórico de decisão Android.
- Desvinculação bilateral e resets graduais no PC e Android.
- Perfil sem segredos, backup completo criptografado, import validado e diagnóstico sanitizado.
- Migração de configuração v2→v3 e retenção de oito backups privados.
- Recovery agregado por Share, filtro contextual, tamanho, exportação/restauração e limpeza manual de resolvidos.
- Último erro por Share e última conexão do dispositivo persistidos.
- Alterações comuns de configuração acordam o modo automático e são aplicadas na rodada seguinte; a pasta SAF ativa fica congelada durante a rodada e manutenção estrutural é serializada no intervalo seguro.

## Verificações estáticas realizadas

- `cargo fmt --all` aceitou e formatou todas as fontes Rust.
- `cargo metadata --no-deps` reconheceu os quatro crates do workspace em 0.4.0.
- `git diff --check` não encontrou erros de whitespace na revisão final.
- Busca de dependências proibidas confirmou que a TUI não usa `DeviceConfig`, `LocalStore`, TLS, journal ou storage diretamente.
- Fluxos Kotlin/JNI/protocolo foram conferidos em conjunto quanto a assinaturas, estados e payload do QR.

## Builds realizados

- `cargo build --release -p rowd`: concluído; binário `rowd 0.4.0` Linux x86_64.
- `scripts/build-android.sh`: concluído; APK `app.rowd` 0.4.0, versionCode 5, minSdk 26 e biblioteca JNI ARM64.

## Testes

Os três testes unitários da TUI passaram. A execução de `cargo test --workspace` dentro do sandbox parou nos testes de integração TLS porque o ambiente recusou a criação de sockets locais (`Operation not permitted`). A repetição fora do sandbox não foi executada por decisão do usuário durante a publicação.

Testes adicionados nesta refatoração incluem:

- breakpoints e resolução de atalhos da TUI;
- migração v2→v3 com backup;
- presença de todas as credenciais no QR compacto;
- ida e volta de backup criptografado e rejeição de senha incorreta;
- retenção do tombstone de solicitação rejeitada.
- remapeamento do simulador local sem remover conteúdo do destino anterior.

## Aceite ainda necessário

1. Executar a suíte completa fora de um sandbox que bloqueie sockets locais e rodar Clippy.
2. Compilar o APK 0.4 e testar leitura física do QR compacto.
3. Confirmar em aparelho cancelamento, aceitação e rejeição após períodos offline.
4. Alterar configuração durante uma transferência grande e verificar aplicação na rodada seguinte.
5. Exercitar remap nas três políticas e revogação iniciada por cada lado.
6. Validar SAF, permissões, recuperação, resets e serviço automático em Android 8 e Android 15+.
