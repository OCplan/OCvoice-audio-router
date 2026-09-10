'use strict';
const $ = id => document.getElementById(id);
const english = Object.fromEntries([...document.querySelectorAll('[data-text]')].map(el => [el.dataset.text, el.textContent]));
const da = {
  eyebrow:'YOUTUBE · DENNE COMPUTER', title:'Forbind computeren, der sender jeres video',
  intro:'Lad appen køre på computeren med OBS eller jeres videoprogram. Den kombinerer jeres video med OCvoices oversatte lyd til jeres YouTube-udsendelser.',
  pairTitle:'Forbind denne computer med OCvoice', pairHelp:'Forbind jeres YouTube-kanal i OCvoice, og få en engangskode til at forbinde en computer. Indtast koden her.',openWeb:'Åbn OCvoice ↗',code:'Engangskode',pair:'Forbind computer',repair:'Vil I bruge en anden OCvoice-konto, skal I først stoppe forbindelsen og derefter indtaste en ny kode.',
  startTitle:'Start forbindelsen',startHelp:'Start forbindelsen her. Vælg derefter sprog, og opret de oversatte udsendelser i OCvoice. Kontrollér deres synlighed, før I sender video.',start:'Start forbindelse',stop:'Stop forbindelse',liveHint:'En kørende forbindelse betyder ikke, at videoen modtages, eller at YouTube er live. Kontrollér udsendelsernes status og seerlinks i OCvoice.',
  videoTitle:'Send video fra OBS',videoHelp:'Åbn OBS → Indstillinger → Stream på denne computer. Vælg Brugerdefineret som tjeneste, og kopiér felterne herunder. Brug H.264-video. Start streaming i OBS, efter I har oprettet udsendelserne i OCvoice.',server:'Server',key:'Streamnøgle',copy:'Kopiér',keyHint:'Dette er et lokalt navn til videoen, ikke jeres YouTube-streamnøgle. Appen finder ikke automatisk jeres kamera. Andre videoprogrammer skal kunne sende RTMP-video med de samme felter.',
  checkTitle:'Kontrollér billede og oversat lyd',checkHelp:'Åbn hvert YouTube-seerlink fra OCvoice. Kontrollér sprog, billede og lyd sammen. Afslut udsendelserne i OCvoice og streaming i OBS, når I er færdige. Stop derefter forbindelsen her.',footer:'Appen skal være åben under udsendelsen. Åbn siden igen fra appens menu “YouTube setup / Opsætning”.'
};
let language = navigator.language.startsWith('da') ? 'da' : 'en';
let state;
let busy = false;
let problemMessage = ['', ''];
const tr = (en, dk) => language === 'da' ? dk : en;
function translate() {
  document.documentElement.lang = language;
  $('language').value = language;
  document.querySelectorAll('[data-text]').forEach(el => { el.textContent = (language === 'da' ? da : english)[el.dataset.text]; });
  problem(...problemMessage);
  render();
}
function problem(en, dk = en) {
  problemMessage = [en, dk];
  const message = tr(en, dk);
  $('problem').textContent = message;
  $('problem').hidden = !message;
}
function render() {
  const running = state && ['running','starting'].includes(state.status);
  const ready = state && state.engineAvailable && state.videoSupportAvailable;
  $('pair').disabled = busy || !ready || running;
  $('code').disabled = busy || running;
  $('start').disabled = busy || !ready || !state.deviceLinked || running;
  $('stop').disabled = busy || !state || state.status === 'stopped';
  if (!state) { $('status').textContent = tr('Checking the app…','Kontrollerer appen…'); return; }
  $('version').textContent = `v${state.version}`;
  $('paired').hidden = !state.deviceLinked;
  $('paired').textContent = tr('Computer connected: ','Computer forbundet: ') + (state.orgName || 'OCvoice');
  $('obs-server').value = state.obsServer;
  $('obs-key').value = state.obsStreamKey;
  $('status').textContent = !ready ? tr('App components missing. Reinstall the complete app.','Appdele mangler. Installér hele appen igen.')
    : running ? tr('Connection running · check video in OCvoice','Forbindelsen kører · kontrollér video i OCvoice')
    : state.status === 'errored' ? tr('Connection interrupted. Stop and start it again.','Forbindelsen blev afbrudt. Stop og start den igen.')
    : state.deviceLinked ? tr('Computer connected · connection stopped','Computer forbundet · forbindelsen er stoppet')
    : tr('Ready to connect this computer','Klar til at forbinde denne computer');
}
async function refresh() {
  const response = await fetch('/restream/setup', {cache:'no-store'});
  if (!response.ok) throw new Error('local app unavailable');
  state = await response.json(); render();
}
async function control(action, body) {
  busy = true; problem(''); render();
  try {
    const response = await fetch(`/restream/${action}`, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body || {})});
    if (action === 'pair') $('code').value = '';
    if (!response.ok) {
      // Do not display engine diagnostics or credential-bearing server bodies.
      throw new Error(action === 'pair' ? 'pair' : 'control');
    }
    await refresh();
  } catch (error) {
    problem(...(error.message === 'pair'
      ? ['Could not connect this code. Check internet access, generate a new code in OCvoice and try again.','Koden kunne ikke forbindes. Kontrollér internetforbindelsen, få en ny kode i OCvoice, og prøv igen.']
      : ['The app could not complete the action. Check that it is open; stop and start the connection again.','Appen kunne ikke udføre handlingen. Kontrollér, at den er åben; stop og start forbindelsen igen.']));
  } finally { busy = false; render(); }
}
$('language').addEventListener('change', event => { language = event.target.value; translate(); });
$('pair-form').addEventListener('submit', event => {event.preventDefault(); control('pair', {code:$('code').value.trim()});});
$('start').addEventListener('click', () => control('start'));
$('stop').addEventListener('click', () => control('stop'));
document.querySelectorAll('[data-copy]').forEach(button => button.addEventListener('click', async () => {
  try { await navigator.clipboard.writeText($(button.dataset.copy).value); button.textContent = tr('Copied','Kopieret'); }
  catch { $(button.dataset.copy).select(); problem('Select and copy the field.','Markér og kopiér feltet.'); }
}));
translate();
async function poll() {
  try { await refresh(); }
  catch { state = undefined; render(); $('status').textContent = tr('Cannot reach the app. Open OCvoice Audio Router and reload this page.','Kan ikke få kontakt til appen. Åbn OCvoice Audio Router, og genindlæs siden.'); }
  setTimeout(poll, 3000);
}
poll();
