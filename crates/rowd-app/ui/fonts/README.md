# Fontes da GUI

- **Instrument Sans Bold (700)**: nome `rowd` junto da marca.
- **Onest Regular (400), SemiBold (600) e Bold (700)**: interface, formulários e
  controles.

Os arquivos são importados pelo Slint e incorporados ao executável; não é
necessária instalação no sistema.

Origem: [Google Fonts / Instrument Sans](https://github.com/google/fonts/tree/main/ofl/instrumentsans)
e [Google Fonts / Onest](https://github.com/google/fonts/tree/main/ofl/onest).
As fontes variáveis originais `InstrumentSans[wdth,wght].ttf` e `Onest[wght].ttf`
foram baixadas em 2026-10-13, sob SIL Open Font License 1.1. Os arquivos usados
pela GUI são instâncias estáticas geradas com FontTools, fixando o eixo `wght`
nos pesos acima e o eixo `wdth` de Instrument Sans em 100. Isso permite que a
medição e a rasterização usem o mesmo desenho no renderizador software do Slint.
Os avisos e as licenças completas estão nos arquivos `InstrumentSans-OFL.txt`
e `Onest-OFL.txt` ao lado das fontes.
