---
name: Rowd
description: Mesa operacional para Shares entre Linux e celular.
colors:
  share-cyan: "#4bcfd2"
  share-cyan-deep: "#184349"
  operational-muted: "#94a3aa"
  healthy-green: "#5ccd8a"
  maintenance-amber: "#efb855"
typography:
  body:
    fontFamily: "terminal monospace"
    fontWeight: 400
    lineHeight: 1
  label:
    fontFamily: "terminal monospace"
    fontWeight: 700
    lineHeight: 1
components:
  share-marker:
    backgroundColor: "{colors.share-cyan}"
    textColor: "#000000"
  selected-share:
    backgroundColor: "{colors.share-cyan-deep}"
    textColor: "#ffffff"
  healthy-status:
    textColor: "{colors.healthy-green}"
  maintenance-status:
    textColor: "{colors.maintenance-amber}"
---

# Design System: Rowd

## Overview

**Creative North Star: "Mesa de Shares"**

O Rowd apresenta sincronização como uma mesa operacional: cada Share tem direção, estado e próxima ação visíveis. A interface mantém uma ordem fixa — aba, seleção, detalhe e ação contextual — para que problemas não se escondam em mensagens transitórias.

A aparência vem do próprio terminal: fundo e fonte são herdados, enquanto ciano marca navegação, verde confirma saúde e âmbar sinaliza manutenção ou espera. Cor nunca é a única evidência de estado; rótulos como ATIVO, PAUSADO, OK e FALHA permanecem obrigatórios.

**Key Characteristics:**

- Painéis master/detail em terminal largo e composição empilhada em terminal estreito.
- Estados operacionais escritos por extenso e ações perigosas protegidas por confirmação textual.
- Superfícies quadradas, bordas simples e ausência deliberada de sombra ou material simulado.
- QR e dados técnicos usam a densidade do terminal sem recorrer a janelas externas.

## Colors

A paleta é um conjunto pequeno de sinais operacionais sobre o fundo escolhido pelo terminal.

### Primary

- **Ciano de Share:** marca a identidade ROWD, a aba ativa e contornos de foco informativo.
- **Ciano Profundo:** sustenta a linha selecionada sem depender de inversão arbitrária do terminal.

### Secondary

- **Verde de Saúde:** reservado a sync ativo, vínculo válido e verificações aprovadas.
- **Âmbar de Manutenção:** identifica pausa, operação em andamento e confirmações que exigem atenção.

### Neutral

- **Cinza Operacional:** texto secundário, contagens e instruções globais.
- **Branco e Preto do Terminal:** fornecem conteúdo principal e contraste do marcador ROWD.

### Named Rules

**The Written State Rule.** Toda mudança de cor de estado vem acompanhada de uma palavra inequívoca; cor isolada não comunica saúde, risco ou seleção.

## Typography

**Display Font:** fonte monoespaçada configurada pelo terminal
**Body Font:** fonte monoespaçada configurada pelo terminal
**Label/Mono Font:** fonte monoespaçada configurada pelo terminal

**Character:** a tipografia é nativa do meio, usada como instrumento para dados e caminhos, não como fantasia “técnica”. Hierarquia vem de peso, caixa e posição, nunca de uma segunda família inventada.

### Hierarchy

- **Title** (peso 700): marca ROWD, aba ativa, seleção e títulos de painel.
- **Body** (peso 400): nomes, caminhos, mensagens e detalhes operacionais.
- **Label** (peso 700, caixa alta quando representa estado): ATIVO, PAUSADO, PENDENTE, OK e FALHA.

### Named Rules

**The Terminal Owns the Typeface Rule.** O Rowd não força uma fonte; respeita a configuração legível do usuário e cria hierarquia com peso e composição.

## Layout

No topo ficam ROWD e o estado curto do celular. Abaixo vêm Shares, Dispositivo e Avançado, seguidos pela mensagem dinâmica de trabalho. O corpo usa lista e detalhe; a versão aparece no canto inferior direito.

Com 110 colunas ou mais, lista e detalhe usam 38/62 da largura. Entre 80 e 109 colunas usam 44/56. Abaixo de 80 colunas, os painéis empilham em 43/57 da altura. A densidade confortável reserva três linhas ao rodapé; a compacta usa duas.

Modais aparecem no centro apenas para foco protegido: QR, ajuda, entrada de dados ou confirmação destrutiva. Navegação e ajuda continuam disponíveis durante trabalhos em segundo plano, enquanto novas mutações aguardam a conclusão.

### Named Rules

**The Share Before Action Rule.** Seleção e detalhes aparecem antes dos comandos; o rodapé prioriza navegação global.

## Elevation & Depth

Não há sombras. Profundidade é comunicada por bordas do terminal, divisão espacial e mudança tonal da seleção. O modal limpa e ocupa a região central, sem simular vidro, relevo ou material físico.

## Shapes

O vocabulário é ortogonal e alinhado à grade de caracteres. Painéis usam borda simples de uma célula; seleção usa preenchimento tonal. Blocos Unicode aparecem apenas na renderização funcional do QR.

## Components

### Header operacional

- **Identity:** marcador ROWD em ciano com texto preto.
- **States:** apenas o estado curto do celular no canto superior direito.
- **Loading:** o nome da operação em andamento substitui a mensagem transitória, sem bloquear navegação.

### Navigation

- **Default:** nome em cinza operacional.
- **Active:** ciano de Share com peso forte.
- **Keyboard:** Tab e Shift+Tab percorrem as três abas; 1, 2 e 3 mudam as seções do Share.

### Route list

- **Shape:** painel com borda simples e itens de uma linha. Solicitações aparecem antes dos Shares.
- **Selected:** fundo ciano profundo, texto branco, peso forte e marcador textual.
- **Empty:** explica o estado e nomeia a ação que cria o primeiro item.

### Detail panel

- **Structure:** rótulo, valor e separação vertical; caminhos e erros podem quebrar linha.
- **Content:** mostra estado atual, último evento persistente e próximos reparos possíveis.

### Input and confirmation modal

- **Focus:** contorno âmbar e título específico da ação.
- **Secret:** senha é mascarada e nunca persistida pela TUI.
- **Destructive state:** exige token textual explícito como REMOVER, DESVINCULAR ou APAGAR TUDO.

### QR modal

- **Primary:** QR denso dentro do terminal, com fingerprint e caminho do SVG privado.
- **Small terminal:** informa as dimensões necessárias e mantém o fallback, sem tentar abrir programa externo.

## Do's and Don'ts

### Do:

- **Do** manter o estado curto do celular visível em todas as abas.
- **Do** adaptar master/detail nos limites de 110 e 80 colunas.
- **Do** nomear problema e recuperação em mensagens de erro.
- **Do** preservar confirmação textual em ações destrutivas.
- **Do** persistir atalhos separadamente da identidade e dos segredos.

### Don't:

- **Don't** transformar listas de Shares em planilhas horizontais largas.
- **Don't** usar cor, glifo ou emoji como única indicação de estado ou ação.
- **Don't** mostrar comandos que não se aplicam à aba corrente.
- **Don't** simular sombra, vidro, relevo ou textura em uma interface de terminal.
- **Don't** esconder segredo, chave privada ou convite em relatórios e perfis comuns.
