# Rowd desktop

A GUI Slint pertence ao pacote `rowd-app`. Seu binário é independente da CLI
`rowd`; usa diretamente a biblioteca de aplicação.

```sh
cargo run -p rowd-app --features gui --bin rowd-app --locked
```

Se já compilado, abra `target/debug/rowd-app`. Em máquinas com pouco espaço,
use `CARGO_PROFILE_DEV_DEBUG=0` ao compilar.

## Funções

- **Pastas:** adicionar, editar, remover o vínculo, pausar, retomar, aceitar ou
  recusar pedidos do Android e solicitar sincronização.
- **Dispositivo:** descobrir celulares, enviar convite e confirmar o código
  recebido no pareamento; consultar conexão e desvincular o celular.
- **Recuperação:** consultar registros, exportar cópias, restaurar uma versão
  ou manter o arquivo atual. Restauração e remoção de vínculos têm confirmação.
- **Configurações:** salvar preferências e exportar diagnóstico. No Linux e Windows,
  integração com bandeja, notificações e inicialização automática do usuário.

Ao abrir, o aplicativo carrega o perfil existente e inicia o servidor de
sincronização. Sem perfil, cria a identidade do PC; nenhum celular é vinculado
até confirmar o pareamento. Os arquivos podem ser sincronizados quando houver
um celular autenticado e pastas habilitadas.

O perfil padrão é `~/.local/share/rowd` no Linux e `%LOCALAPPDATA%\Rowd` no Windows;
`ROWD_HOME` ou `--profile caminho` permitem escolher outro.
A GUI e um daemon existente não podem executar o servidor simultaneamente no
mesmo perfil. Nesse caso a janela mostra o erro; encerre o outro servidor e
use **Tentar novamente** em Configurações.

A conexão é mostrada a partir dos sinais de autenticação do motor. O botão
Verificar conexão apresenta verificações locais, sem declarar que o celular
está conectado. O protocolo não informa o caminho remoto da pasta ao PC;
a escolha desse caminho permanece no Android.

As preferências ficam em `.rowd/gui-preferences.json`. Fechar a janela mantém
o processo apenas quando há uma bandeja disponível e a opção está habilitada.
Sem bandeja, fechar encerra o servidor. O menu **Sair** encerra o aplicativo.

## Organização

- `src/main.rs`: entrada exclusiva da GUI.
- `src/gui.rs`: modelos e callbacks Slint; seleção por identificadores estáveis.
- `src/gui/controller.rs`: comandos e servidor em segundo plano.
- `src/gui/desktop.rs`: preferências e integrações do desktop Linux.
- `src/gui/windows.rs`: bandeja, notificações e inicialização automática Windows.
- `ui/app.slint`: telas, formulários e confirmações.
- `ui/components.slint`: componentes e cores aprovados.
- `ui/fonts/`: Instrument Sans Bold para a marca; Onest para a interface,
  incorporadas ao executável com suas licenças.

As dependências gráficas são opcionais, ativadas pela feature `gui`. O Slint
está na série 1.14 para coexistir com as dependências atuais do workspace.
O port Windows possui executável exclusivo da GUI, com compilação MSVC e
empacotamento em `scripts/build-windows.ps1`. Consulte [WINDOWS.md](WINDOWS.md)
para uso e limitações de validação em ambiente real.

Slint é usado sob a opção **Royalty-free Desktop, Mobile, and Web Applications
License**, com atribuição em Configurações. Consulte
[os termos do Slint](https://slint.dev/terms-and-conditions).
