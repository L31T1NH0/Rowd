# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Users

Pessoas que usam um PC Linux e um aparelho Android e querem administrar a sincronização direta de seus próprios arquivos pela rede local.

## Product Purpose

O Rowd sincroniza múltiplos pares de pastas, chamados Shares, entre um PC Linux e um Android sem servidor ou nuvem intermediária. Sucesso significa que pareamento, configuração, sincronização, conflitos e recovery possam ser compreendidos e administrados nos dois dispositivos sem perder arquivos.

## Positioning

Um único vínculo PC ↔ Android, autenticado por TLS, pinning e HMAC, coordena Shares independentes e preserva versões conflitantes e recovery localmente, sem entregar os arquivos a um serviço externo.

## Operating Context

- TUI Ratatui em terminais Linux largos ou estreitos.
- Aplicativo Android nativo usando Storage Access Framework.
- Rede local, com o Android iniciando rodadas de sincronização.
- Configuração e estado persistidos em arquivos locais atômicos.
- Operação frequente em modo automático, inclusive com um dispositivo temporariamente offline.

## Capabilities and Constraints

- Um PC e um Android por vínculo.
- Múltiplos Shares, cada um com identidade, modo, journal, estado e recovery próprios.
- Modos bidirecional, PC → Android e Android → PC.
- Arquivos inteiros e transferência sequencial; sem blocos ou retomada parcial.
- Sem daemon, mDNS, conexão TCP permanente ou múltiplos dispositivos nesta etapa.
- A futura GUI desktop será outro frontend sobre a mesma camada de aplicação e não dependerá de Ratatui.

## Brand Commitments

- Nome: Rowd.
- Frase factual existente: “Seus arquivos, entre seus dispositivos.”
- Ícones existentes em `assets/icons/rowd-pc.png` e `android/app/src/main/assets/icons/rowd-mobile.png`.
- Voz direta, técnica e tranquilizadora; ações destrutivas explicam exatamente o que será preservado.

## Evidence on Hand

- Motor Rust existente para reconciliação, TLS/HMAC, journal, storage e recovery.
- Frontend Ratatui e aplicativo Android/Kotlin existentes.
- Plano aprovado em `prompts/ROWD_PLANO_FASES_REFATORACAO_TUI.md`.
- Não há depoimentos, métricas comerciais ou claims externos a inventar.

## Product Principles

1. Segurança e preservação de dados não podem regredir por conveniência de interface.
2. Frontends traduzem intenção; a camada compartilhada executa regras e persistência.
3. Administração normal usa pausar, reparar, remapear, desvincular e restaurar antes de apagar.
4. Estado e falhas devem permanecer legíveis depois que uma mensagem transitória desaparece.
5. Cada mudança perigosa deve ser validada, salva atomicamente e recuperável quando possível.

## Accessibility & Inclusion

Atalhos devem ser descobertos no contexto, ações não podem depender apenas de cor e a TUI deve permanecer operável em terminais com menos de 80 colunas.
