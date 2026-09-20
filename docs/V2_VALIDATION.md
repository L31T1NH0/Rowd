# Validação da implementação V2

Referência: `prompts/output/AVALIACAO_IMPLEMENTACAO_V2.md`. Registro de 20/09/2026.

## Implementação

- Configuração de dispositivo em JSON atômico e estado isolado por `share_id`.
- Migração explícita da identidade/base V1; recovery permanece na pasta original.
- Protocolo 2 independente do formato de convite 1 e do pacote 0.2.0.
- Configuração e remoção de Shares pelo PC; remoção não apaga conteúdo.
- Solicitações de Share iniciadas no Android ficam pendentes até o usuário escolher e confirmar a pasta no PC.
- Mensagens de sincronização encapsuladas com `share_id`; IDs desconhecidos são recusados.
- Manifesto e journal persistentes em ambos os lados; ACK associado a caminho e versão; compactação por caminho.
- Watcher Linux com debounce, cache validado por metadados, fallback e scan completo manual.
- Android com ContentObserver como sinal de mudança e polling SAF como capacidade explícita.
- TUI Ratatui e CLI usam os mesmos serviços; QR e JSON transportam a mesma credencial.
- `.rowdignore`, modos e acesso ao recovery no PC/Android; exclusões continuam sem propagação.

## Testes automatizados

A suíte original de 14 testes passou antes das alterações. A suíte ampliada verifica:

- TLS, HMAC, segredo incorreto e identidade de raiz única;
- SHA-256, truncamento, escrita condicional, `STALE_TARGET` e recovery;
- convergência bidirecional e conflitos idempotentes;
- dois Shares com o mesmo nome de arquivo e estados separados;
- reinício entre rodadas e pendência offline persistente;
- renomeação do Share sem trocar ID ou mover destino;
- rejeição de raízes sobrepostas;
- migração sem perder credenciais ou cópias de recovery;
- burst de eventos com o Android offline;
- ignorados fora das transferências em ambos os sentidos;
- confirmação obrigatória de remoção e retenção dos arquivos;
- três modos, mudanças concorrentes preservadas e reinício de estado;
- conexão interrompida depois de instalar e antes do ACK, reenvio sem duplicação;
- compactação A/B/C/D e ACK de versão antiga/duplicado.

Os testes TCP/TLS/Unix precisaram executar fora do sandbox, que bloqueia criação de sockets. Não foi removido nenhum teste por causa dessa restrição.

## Limites da validação

O ADB não encontrou dispositivo conectado. Não há emulador instalado. Não foram executados testes de câmera/QR físico, permissões e comportamento de provedores SAF reais, limites do serviço de primeiro plano, bateria, uso por horas offline ou interrupção física do aparelho durante gravação.

O cache persistente Linux evita rehash obrigatório ao reabrir; o SAF usa hashes completos quando não há observação confiável. A TUI mostra progresso por caminhos, sem progresso byte a byte. O PC espera a próxima sessão iniciada pelo Android para entregar mudanças; não há canal permanente de aviso.

## Aceite em aparelho, ainda necessário

1. Instalar o APK ARM64, escolher uma raiz Rowd e parear por QR. Repetir em uma instalação de teste por JSON.
2. Cadastrar dois Shares no PC e verificar criação automática, conflito, rename visual e remoção sem apagar dados.
3. Editar dos dois lados, desligar Wi-Fi por horas, reiniciar ambos e verificar pendências/retomada.
4. Interromper processo/rede durante upload e download, inclusive após a instalação, antes do ACK.
5. Usar um provider sem eventos e verificar o fallback; revogar permissão e verificar mensagem de correção.
6. Conferir `.rowdignore`, modos unidirecionais, exportação/restauração e bloqueio por recovery ambíguo.
7. Testar câmera, tema claro/escuro, TalkBack, texto ampliado e limite de serviço em Android 15+.

A implementação não deve ser considerada validada em aparelho antes de concluir essa lista.
