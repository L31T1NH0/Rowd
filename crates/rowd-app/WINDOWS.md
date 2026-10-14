# Rowd para Windows

O executável `Rowd.exe` abre exclusivamente a GUI Slint. O motor P2P funciona
no mesmo processo; não há instalação de CLI, daemon Linux ou serviço Windows.
Fontes e recursos da interface estão incorporados ao executável.

## Compilar no Windows

Use Windows 10/11 x64 com Rust MSVC e Visual Studio Build Tools, incluindo
as ferramentas C++ e o Windows SDK. Na raiz do repositório:

```powershell
./scripts/build-windows.ps1
```

O resultado fica em `dist/windows-x64/Rowd.exe`, acompanhado das licenças.
A compilação usa o runtime C estático. Mantenha o executável em um caminho
estável antes de habilitar a inicialização automática. Após mover o aplicativo,
reative essa opção para atualizar o caminho.

O job Windows do CI compila apenas `rowd-app` com `gui` e disponibiliza a
pasta de distribuição como artefato. Os demais frontends não integram o pacote.

## Usar

Abra `Rowd.exe`. O perfil padrão fica em `%LOCALAPPDATA%\Rowd`; credenciais,
preferências e registros ficam nesse perfil, separado do executável.
O aplicativo usa permissões herdadas da pasta do usuário.

`ROWD_HOME` ou `Rowd.exe --profile "C:\caminho\perfil"` permitem escolher outro
perfil. A inicialização automática preserva esse caminho no registro do usuário
(`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) e não exige administrador.

No Android, abra Encontrar na rede e confirme o mesmo código nos dois aparelhos.
Permita o Rowd em **redes privadas** no Firewall do Windows. As portas padrão são
TCP 43821 (conexão autenticada), UDP 43822 (descoberta) e UDP 43823 (pareamento).
O aplicativo não cria regras de firewall automaticamente.

O ícone da bandeja oferece Abrir, Pausar/retomar e Sair. A opção de continuar
ao fechar depende de uma bandeja disponível. As notificações usam a área de
notificação do Windows e respeitam a preferência do Rowd e as opções do sistema.
Fechar a janela com essa opção desativada encerra a sincronização.

Arquivos abertos por programas que impedem renomeação podem gerar uma falha
na pasta. Feche o arquivo e solicite novamente a sincronização. Arquivos locais
e cópias de recuperação não são descartados para forçar a substituição.
O sistema de arquivos precisa fornecer identidade e metadados dos arquivos
pelas APIs do Windows; falhas nessa consulta interrompem a operação.

## Estado do port

A implementação inclui perfil por usuário, bandeja, notificações, inicialização
automática e identificação dos arquivos pelo volume, ID de 128 bits, tamanho,
data de escrita e data de alteração. A varredura incremental permanece
habilitada apenas no Unix; no Windows o motor usa a varredura completa e o cache
com metadados nativos.

A primeira compilação GNU x64 foi gerada por cross-compilation no Linux, em
perfil de desenvolvimento. O formato PE foi inspecionado: x64, subsistema GUI
e importações de DLLs do próprio Windows. O pipeline MSVC release está preparado,
mas ainda não foi executado.

Compilação e funcionamento real são evidências distintas. Pareamento,
transferências, recuperação, reinício do Explorer e encerramento com arquivos
abertos ainda precisam de validação em um Windows real com Android.

Slint é usado sob a opção **Royalty-free Desktop, Mobile, and Web Applications
License**, com atribuição em Configurações. Consulte
[os termos do Slint](https://slint.dev/terms-and-conditions).

Referências das integrações nativas:
[área de notificação](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw),
[informações de arquivos](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileinformationbyhandleex).
