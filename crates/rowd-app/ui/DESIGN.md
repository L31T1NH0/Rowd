---
name: Rowd desktop
description: Interface desktop clara para administrar pastas entre PC e Android.
colors:
  canvas: "#e2ebe8"
  content: "#f7faf9"
  surface: "#ffffff"
  ink: "#12282a"
  muted: "#435a5d"
  line: "#b6c8c2"
  accent: "#0b6667"
  selected: "#c9e5df"
  healthy: "#1f6542"
  warning: "#81500e"
  service-banner: "#d5e1dc"
  request-area: "#dceee7"
  path-area: "#edf3f0"
typography:
  brand:
    fontFamily: "Instrument Sans"
    fontSize: "29px"
    fontWeight: 700
  headline:
    fontFamily: "Onest"
    fontSize: "24px"
    fontWeight: 700
  title:
    fontFamily: "Onest"
    fontSize: "21px"
    fontWeight: 700
  body:
    fontFamily: "Onest"
    fontSize: "14px"
    fontWeight: 400
  caption:
    fontFamily: "Onest"
    fontSize: "12px"
    fontWeight: 400
rounded:
  status: "4px"
  item: "6px"
  panel: "8px"
  modal: "10px"
  content: "14px"
spacing:
  direction: "8px"
  sidebar: "16px"
  scrollbar-reserve: "20px"
  main: "24px"
components:
  action-button:
    height: "34px"
  sidebar:
    width: "200px"
  folder-list:
    width: "260px"
---

# Design System: Rowd desktop

## Overview

Interface funcional Slint em `rowd-app`, com estilo Fluent claro e identidade
visual aprovada. A GUI tem binário próprio, separado da CLI, e usa a camada
de aplicação e o motor reais. A implementação Windows usa o mesmo binário GUI,
com integrações de desktop próprias; sua validação em ambiente real permanece pendente.
Esta especificação registra a GUI construída. O
`DESIGN.md` da raiz descreve a interface de terminal e tem escopo próprio.

A pasta selecionada organiza detalhe e ação. Estados escritos, caminho local,
referência à pasta no Android e direção tornam a sincronização compreensível. A marca
existente em `assets/icons/rowd-pc.png` e a frase “Seus arquivos, entre seus
dispositivos.” mantêm a identidade do produto.

O banner permanente mostra “Rowd ativo” ou “Rowd indisponível”, preparação
do perfil e erros reais do servidor. Callbacks enviam comandos ao controller;
operações de arquivos e rede e o servidor rodam em background. Listas e
estados vêm de snapshots da aplicação e sinais do motor, sem mocks.
Preferências são carregadas e salvas no perfil. O código e o nome exibidos
na confirmação de vínculo vêm do pedido real de pareamento.

## Colors

O acento verde azulado identifica seleção, direção e foco. Canvas verde suave,
conteúdo quase branco, painéis brancos e texto escuro reforçam o contraste. Texto
secundário usa `muted`; saúde usa `healthy`, pausa e atenção usam `warning`.
Os nomes e valores normativos estão no frontmatter e correspondem a `Theme`
em `components.slint`.

Banner, solicitação e caminhos usam fundos próprios do frontmatter. A lateral
compartilha o canvas; o painel principal usa `content` e contorno `line`.

**Estado escrito.** Cor acompanha palavras como “Sincronizada”, “Pausada”,
“Sincronizando” e “Precisa de atenção”; nunca comunica estado sozinha.

## Typography

Instrument Sans Bold (700) aparece somente no nome `rowd` da lateral. Onest
é a família padrão da janela, dos formulários e dos controles, com instâncias
estáticas Regular (400), SemiBold (600) e Bold (700).

Títulos de página, detalhe, corpo e legendas seguem os papéis do frontmatter.
Nomes de pasta, campos de caminho e rótulos de estado usam peso 600. Caminhos
quebram linha; nomes na lista podem receber elipse.

As quatro fontes são importadas em `app.slint` e incorporadas pelo `build.rs`
com os recursos da GUI. Não dependem de instalação no sistema. Origem,
geração das instâncias e licenças SIL OFL 1.1 estão em `fonts/README.md`,
`fonts/InstrumentSans-OFL.txt` e `fonts/Onest-OFL.txt`.

## Layout

A janela declara tamanho lógico preferido de 1140 × 790 e mínimo de
880 × 650. O banner superior tem 36 px. A lateral fixa contém Pastas,
Dispositivo, Recuperação e Configurações, com vínculo do celular no rodapé.
Seu padding é 16 px; o conteúdo principal usa 24 px.
O corpo reserva insets de 8 px no topo, embaixo e à direita; o painel principal
tem contorno de 1 px e raio de 14 px.

Pastas usa lista fixa à esquerda e detalhe expansível à direita. Lateral e
lista mantêm as larguras do frontmatter: não há breakpoint que as reduza.
Lista e detalhe recebem rolagem; as ações “Sincronizar agora” e
“Pausar/Retomar” ficam no rodapé do detalhe, fora da área rolável. Dois blocos
exibem o caminho no PC e a referência à pasta gerenciada no Android: o snapshot
atual não fornece o caminho remoto, e a interface informa essa limitação.
A linha de direção reserva 8 px entre ícone
e texto. Configurações reserva 20 px à direita do conteúdo para a scrollbar.

Formulários, confirmação de vínculo e confirmação de ações aparecem em modais centralizados.
Dispositivo e Recuperação usam painéis com conteúdo rolável.

A revisão visual foi concluída nas capturas Linux em tamanho nativo
1362 × 729: Pastas, Dispositivo, Recuperação, Configurações e formulário.
Fontes e controles compactos foram confirmados; os dois ajustes finais de
espaçamento foram considerados resolvidos. A tentativa de redimensionar foi
ignorada pelo gerenciador de janelas. Não houve validação visual em janela
estreita nem em Windows, e não existe composição de imagem aprovada.
A revisão da etapa funcional atual é de código e build; não constitui nova
validação visual nem validação P2P. As capturas anteriores não comprovam os
fluxos atuais com dados reais, nem os refinamentos visuais posteriores.

## Elevation & Depth

Não há sombras ornamentais. Divisórias de 1 px, bordas e diferenças de tom
separam regiões. Modais usam superfície branca sobre um véu escuro translúcido.

## Shapes

Itens e blocos de caminho têm cantos suaves; painéis e modais usam raios
maiores, conforme o frontmatter. Estados usam cápsulas retangulares de raio
pequeno. Controles nativos seguem o estilo Fluent do Slint em esquema claro.

## Components

- **ActionButton:** envolve o Button Fluent, limita a altura a 34 px e usa
  largura conforme o conteúdo, sem expansão horizontal. A variante primária
  destaca a ação principal; estados desabilitados seguem disponibilidade.
- **NavItem:** linha de 44 px com caixas fixas de 20 px para ícone e texto;
  ícone em x=13, texto em x=43 e ajuste óptico vertical de +1 px. Rótulos usam
  Onest SemiBold (600) em ambos os estados, mantendo alinhamento na seleção.
  Seleção por ponteiro usa preenchimento tonal sem borda; foco por teclado
  usa sublinhado de 1 px. Enter e Espaço ativam a seção.
- **ShareItem:** linha de 90 px com nome, direção e estado escrito; seleção,
  hover e foco acompanham navegação por teclado.
- **StatusLabel:** altura de 27 px, peso 600 e preenchimento verde ou âmbar
  suave, sempre com descrição textual.
- **PathDisplay:** ícone, rótulo secundário e caminho em peso 600, com padding
  de 16 px e quebra de linha; a referência Android informa quando o caminho
  remoto não está disponível.
- **Formulários:** LineEdit e ComboBox Fluent, erros escritos junto aos campos
  e seletor nativo de pasta. Modais bloqueiam a navegação de fundo; ações e
  fechamento por Escape respeitam operações em andamento.
- **Pareamento e confirmação:** código real do pedido recebido, decisão de
  aceitar/recusar e mensagens de erro. Remover vínculo, desvincular celular e
  decidir recuperação exigem confirmação com consequências escritas.
- **Recuperação:** registros reais, seleção de versão, restauração, exportação
  por diálogo nativo e manutenção da versão atual; disponibilidade condiciona ações.
- **Configurações:** Switches Fluent com preferências persistentes no perfil;
  bandeja e autostart respeitam disponibilidade do desktop. Sem bandeja,
  fechar a janela encerra o Rowd. Exibe o perfil e oferece exportar diagnóstico
  ou tentar iniciar novamente o servidor após erro.

## Do's and Don'ts

- **Do** preservar a marca existente, as fontes incorporadas e o banner de estado real.
- **Do** manter caminho local, referência Android, direção e estado legíveis antes da ação.
- **Do** manter ações compactas e o rodapé do detalhe fora da rolagem.
- **Don't** inventar caminhos remotos ou apresentar uma solicitação de sync como transferência concluída.
- **Don't** declarar validação Windows, adaptação de colunas ou revisão em janela
  estreita sem evidência adicional.
