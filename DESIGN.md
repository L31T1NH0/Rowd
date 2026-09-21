---
name: Rowd
description: Mesa operacional para rotas de sincronização entre Linux e Android.
colors:
  route-cyan: "#4bcfd2"
  route-cyan-deep: "#184349"
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
  route-marker:
    backgroundColor: "{colors.route-cyan}"
    textColor: "#000000"
  selected-route:
    backgroundColor: "{colors.route-cyan-deep}"
    textColor: "#ffffff"
  healthy-status:
    textColor: "{colors.healthy-green}"
  maintenance-status:
    textColor: "{colors.maintenance-amber}"
---

# Design System: Rowd

## Overview

**Creative North Star: "Mesa de Rotas"**

O Rowd apresenta sincronização como uma mesa operacional: cada Share é uma rota com origem, destino, estado e próxima ação visíveis. A interface é densa o bastante para administrar várias rotas, mas mantém uma ordem fixa — saúde global, categoria, seleção, detalhe e ação contextual — para que problemas não se escondam em mensagens transitórias.

A aparência vem do próprio terminal: fundo e fonte são herdados, enquanto ciano marca navegação, verde confirma saúde e âmbar sinaliza manutenção ou espera. Cor nunca é a única evidência de estado; rótulos como ATIVO, PAUSADO, OK e FALHA permanecem obrigatórios.

**Key Characteristics:**

- Painéis master/detail em terminal largo e composição empilhada em terminal estreito.
- Estados operacionais escritos por extenso e ações perigosas protegidas por confirmação textual.
- Superfícies quadradas, bordas simples e ausência deliberada de sombra ou material simulado.
- QR e dados técnicos usam a densidade do terminal sem recorrer a janelas externas.

## Colors

A paleta é um conjunto pequeno de sinais operacionais sobre o fundo escolhido pelo terminal.

### Primary

- **Ciano de Rota:** marca a identidade ROWD, a aba ativa e contornos de foco informativo.
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

O primeiro bloco tem duas linhas persistentes: identidade/estado do dispositivo e contagens/mensagem de trabalho. Abaixo vêm cinco abas numeradas, corpo e rodapé contextual.

Com 110 colunas ou mais, lista e detalhe usam 38/62 da largura. Entre 80 e 109 colunas usam 44/56. Abaixo de 80 colunas, os painéis empilham em 43/57 da altura. A densidade confortável reserva três linhas ao rodapé; a compacta usa duas.

Modais aparecem no centro apenas para foco protegido: QR, ajuda, entrada de dados ou confirmação destrutiva. Navegação e ajuda continuam disponíveis durante trabalhos em segundo plano, enquanto novas mutações aguardam a conclusão.

### Named Rules

**The Route Before Action Rule.** Seleção e detalhes aparecem antes dos comandos; o rodapé nunca oferece ações de outra aba.

## Elevation & Depth

Não há sombras. Profundidade é comunicada por bordas do terminal, divisão espacial e mudança tonal da seleção. O modal limpa e ocupa a região central, sem simular vidro, relevo ou material físico.

## Shapes

O vocabulário é ortogonal e alinhado à grade de caracteres. Painéis usam borda simples de uma célula; seleção usa preenchimento tonal. Blocos Unicode aparecem apenas na renderização funcional do QR.

## Components

### Header operacional

- **Identity:** marcador ROWD em ciano com texto preto.
- **States:** sync e Android escritos por extenso, seguidos por contagens e mensagem corrente.
- **Loading:** o nome da operação em andamento substitui a mensagem transitória, sem bloquear navegação.

### Navigation

- **Default:** número e nome em cinza operacional.
- **Active:** ciano de rota com peso forte.
- **Keyboard:** 1..5 abre diretamente; Tab e Shift+Tab percorrem a sequência.

### Route list

- **Shape:** painel com borda simples e itens de duas linhas.
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

- **Do** manter saúde global e vínculo visíveis em todas as abas.
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
