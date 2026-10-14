# Memonotes - product- en implementatieplan

Oorspronkelijk plan: 2026-10-05. Stand bijgewerkt: 2026-10-13.
Status: GTK-desktopapp met SQLite, meerdere Memos-accounts, achtergrondophalen en
automatische outbox-uploads. Sync bewaart basisversies, detecteert conflicten,
reconcilieert bevestigde remote deletions en herstelt onzekere creates. De editor
ondersteunt WYSIWYG-opmaak en lokale inline afbeeldingen; Memos-attachment-sync,
automatisch samenvoegen en live multi-client racevalidatie blijven open.
Projectmap: /home/fedora/repos/memonotes.
De onderstaande productafspraken beschrijven het doel; de checklist geeft aan wat
daadwerkelijk in de huidige code is gerealiseerd. Zie ook [README.md](README.md).

## Huidige stand en todo's

### Gerealiseerd

- [x] Rust/GTK4/libadwaita-app met drie verstelbare panelen en inklapbare zijbalk.
- [x] SQLite-opslag, lokale autosave, folders, zoeken, pinnen en archief/terugzetten.
- [x] Gearchiveerde notities definitief wissen met bevestigingsdialoog en `NoteDeleted` via de adapter naar de Memos DELETE-API; geen undo voor definitief wissen.
- [x] Undo voor archiveeracties; undo is niet persistent.
- [x] Meervoudige notitieselectie en slepen naar een map.
- [x] Losse bewerkbare notitievensters en toetsenbordnavigatie/sneltoetsen.
- [x] 32 notitiekleuren met licht/donker-varianten en een Over-venster.
- [x] Meerdere accounts beheren, inschakelen/uitschakelen, verbinding testen en PAT in GNOME Keyring bewaren.
- [x] Memos-adapter met backend-onafhankelijke notitie- en foldermodellen; IMAP/Joplin zijn alleen keuzemogelijkheden, niet geimplementeerd.
- [x] Memos-notities/spaces gepagineerd ophalen en transactioneel cachen per account (schema v11: remote-ID-links, sync-baselines en conflicts).
- [x] Ophalen bij opstart, accountopslaan/inschakelen en elke 60 seconden bij zichtbaar hoofdvenster.
- [x] Geselecteerde backendnotitie apart ophalen wanneer geen lokale notitie-events pending zijn.
- [x] Bij openen van een Memos-map de space via GET en actieve notities met space-filter gepagineerd ophalen; pending lokale wijzigingen blijven beschermd.
- [x] Duurzame outbox, accountrouting, remote-ID-links en bevestiging na succesvolle upload.
- [x] Automatische uploads, een seconde debounce na tekstbewerking, opeenvolgende updates samenvoegen en capped exponential retry (30 seconden tot maximaal 15 minuten).
- [x] Automatische netwerkdetectie zonder handmatige modus: offline API-calls pauzeren en na reconnect eerst ophalen/vergelijken, daarna veilige outbox-events uploaden.
- [x] API-mapping voor notitie-CRUD/herstel, pinnen, archiveren, verplaatsen en mapaanmaak/hernoemen/verwijderen.
- [x] Core Memos-integratie live getest tegen de doelserver en werkend.
- [x] Veilige map-/spaceverwijdering met drie dialoogkeuzes: annuleren, map verwijderen met behoud van notities, of map en alle notities verwijderen. Note-moves/-deletions worden voor `FolderDeleted` gequeued.
- [x] Titel en body vormen één gesynchroniseerd Markdown-document. Legacy-notities met losse titel worden bij bewerken naar een eerste H1 omgezet zonder de body te verliezen; ongewijzigde inhoud maakt geen nieuw sync-event.
- [x] Editor toont inline vet/cursief/onderstrepen als opmaak, koppen H1-H6 en alineakeuze via de werkbalk, aanklikbare checklists, niet-selecteerbare bulletmarkeringen, GTK undo/redo en lokaal ingevoegde inline afbeeldingen die selecteerbaar, versleepbaar en via de hoeken te schalen zijn (verhouding blijft behouden); opslaan vertaalt dit terug naar Markdown. Afbeeldingsbestanden staan app-privé en gebruiken een lokale verwijzing.
- [x] Drieweg-conflictdetectie bewaart basis, lokale en remote versie; de gebruiker kiest lokaal of server. Lokale deletes die botsen met serverwijzigingen zijn ook oplosbaar.
- [x] Complete accountrefreshes bevestigen ontbrekende memo's met een directe GET; alleen 404 geldt als remote delete. Pending lokale wijzigingen worden als conflict bewaard.
- [x] Onzekere creates en HTTP 409 reconciliëren via de stabiele memo-ID; alleen een exact overeenkomende remote resource wordt bevestigd.
- [x] Handmatig synchroniseren en capped exponential retry voor tijdelijke fouten (30 seconden tot maximaal 15 minuten).
- [x] API-fouttoasts met deduplicatie; achtergrondophalen/autosave zonder voortgangstoasts.
- [x] Opslag/sync-tests en HTTP-mocktests; API-probe beschikbaar.

### Nog te doen

- [x] Alle 2.700 notities gepagineerd ophalen en correct sorteren; live bevestigd.
- [x] Archiveren/terugzetten werkt remote; end-to-end getest op de echte server.
- [x] Gedeelde Memos-spaces en niet-private notities uit de UI filteren; gedeelde data alleen in de sync-cache behouden.
- [x] GTK-editor undo/redo met focusbewuste Ctrl+Z-routering en bijgewerkte knopstatus.
- [ ] WYSIWYG-editor afronden: overige blokopmaak en verdere end-to-end roundtrips testen. Inline opmaak, koppen/alinea's, checklist-, bullet-, link-, lokale afbeelding- en undo/redo-acties zijn aangesloten.
- [ ] Memos-attachments live testen: uploaden (met koppeling aan de memo), downloaden en lokaal cachen zijn gebouwd en met mocktests gedekt (attachment-ID = lokale UUID, inhoud verwijst naar `/file/attachments/<id>/<bestand>`); live validatie tegen de doelserver, het opruimen van niet meer gebruikte attachments en afbeeldingen die buiten Memonotes zijn toegevoegd ontbreken.
- [ ] Markdown-export met folders, metadata en afbeeldingen.
- [ ] Expliciete onboarding/lokale import en veilig ontkoppelen met pending wijzigingen/export.
- [ ] Smalle-vensternavigatie, onthouden paneelbreedtes/selectie, toegankelijkheid en zichtbare sync/conflictstatus.
- [ ] Fedora/Flatpak-verpakking en netwerk/keyringtests in de sandbox.

Niet gebouwd en buiten de huidige versie 1-scope: space-leden/uitnodigingen,
publiceren, PAT-beheer via de API, mobiel en React Native. De precieze
attachment-functionaliteit moet nog worden onderzocht.

## 1. Bevestigde conclusies

- De app heet Memonotes en is gericht op de Linux/GNOME-ervaring, met Fedora als eerste doelplatform.
- Desktoptechniek: Rust, GTK4 en libadwaita.
- Apple Notes is het visuele referentiepunt: zijpaneel, notitielijst en notitiedetail/editor.
- Geen submappen: in Memos-modus is een space precies een folder met daarin notes.
- Persoonlijke groepering is het doel; samenwerking is geen doel van versie 1.
- De hoofdeditor is WYSIWYG zoals Apple Notes, niet een Markdown-code-editor.
- Checklists/todo's met aanklikbare checkboxes, bold, italic en afbeeldingen horen bij versie 1.
- Onderstrepen wordt als beperkte `<u>...</u>`-extensie geschreven; compatibiliteit en weergave op de doelserver moeten live worden getest.
- Een optionele Markdown-brontab mag, maar is niet nodig voor normaal gebruik.
- Zonder backend werkt de app volledig lokaal.
- Met backend gebruikt de app de bestaande Memos-API op Umbrel.
- De gebruiker maakt zelf een app key aan in Memos en voert die in Memonotes in.
- Uitgangspunt voor compatibiliteit: Memos 0.31.0.
- Ook in Memos-modus moet offline lezen en bewerken mogelijk zijn, met latere synchronisatie.
- Mobiel en React Native vallen buiten dit projectplan; ze bepalen nu geen architectuur of planning.

## 2. Vormgeving en navigatie

Doel: dezelfde herkenbare indeling en rustige interactie als Apple Notes.
GTK/libadwaita levert wel Fedora-eigen vensterbediening, widgets en toegankelijkheid.
Pixel-identieke macOS-controls worden niet automatisch beloofd; eigen branding en
beschikbare Linux-fonts gebruiken, geen Apple-assets of Apple-fonts meeleveren.

Voorgestelde brede desktopindeling:

```text
| Zijpaneel           | Notitielijst          | Detail / editor             |
|--------------------|-----------------------|-----------------------------|
| Op dit apparaat    | Zoeken                | Actiebalk                   |
| of Memos-account   | Vastgepinde notities  | Titel                       |
| Alle notities      | Titel, datum, snippet | Datum en opslagstatus       |
| Zonder map         | Geselecteerde notitie | Notitie-inhoud              |
| Mappen / spaces    | Overige notities      |                             |
| Archief            |                       |                             |
| Instellingen       |                       |                             |
```

- Verstelbare paneelbreedtes en onthouden selectie.
- Bij smalle vensters opeenvolgende schermen met terugnavigatie in plaats van geplette kolommen.
- Acties: nieuwe notitie, nieuwe map, verplaatsen, vastpinnen, archiveren en zoeken.
- Lokale autosave met herkenbare status: opgeslagen, wacht op sync, offline, conflict of fout.
- Licht/donker volgens systeeminstelling; bediening met toetsenbord en screenreader.
- WYSIWYG als standaard: opgemaakte inhoud direct bewerken, zonder zichtbare Markdown-syntax.
- Checklists/todo's met checkboxes die direct af te vinken zijn; status wordt opgeslagen en gesynchroniseerd.
- Opmaakacties voor bold en italic, met herkenbare bediening en actieve selectiestatus.
- Afbeeldingen invoegen en inline weergeven; bestandskeuze en plakken ondersteunen.
- Undo/redo, selectie en normale toetsenbordbediening horen bij de editorproef.
- Nieuwe notities slaan de titel als eerste H1 op voor Memos-compatibiliteit; bij bestaande notities blijft de inhoud ongewijzigd tot de gebruiker ze bewerkt.
- Voorbeeldloze of lege notities krijgen een lokale weergavetitel; bestaande inhoud niet herschrijven.

Markdown blijft het uitwisselingsformaat met Memos, niet de normale gebruikersinterface.
De editor gebruikt een bewezen rich-text-engine; de concrete GTK-integratie wordt in een
klein prototype gekozen. GTK/libadwaita alleen levert niet automatisch deze complete editor.
Eerst aantonen dat checklists, opmaak en afbeeldingen na opslaan en opnieuw openen behouden blijven.
Geen opmaak stilzwijgend strippen of alleen lokaal bewaren als de server deze niet kan behouden.

Een Markdown-brontab is optioneel en nog niet besloten. Als die wordt toegevoegd, bewerken
beide tabs dezelfde notitie; tabwisseling behoudt inhoud en toont geen verouderde versie.
Onbekende Markdown/HTML uit Memos niet stilzwijgend verliezen bij een WYSIWYG-roundtrip.
Onondersteunde inhoud intact bewaren en gericht waarschuwen als visuele bewerking niet veilig kan.

## 3. Opslagkeuze en onboarding

Eerste start bevat een expliciete keuze:

1. Op dit apparaat: meteen starten, zonder account, sleutel of netwerkverzoeken.
2. Memos verbinden: serveradres en persoonlijke app key invoeren, verbinding testen.

In Instellingen > Opslag blijft het mogelijk later Memos te koppelen of los te koppelen.
Huidige implementatie: meerdere Memos-accounts met aparte collecties; lokale notities
blijven apart. De oorspronkelijke beperking tot een account is niet toegepast.
Koppelen uploadt bestaande lokale notities nooit stilzwijgend. Een expliciete kopieer/importactie
toont selectie en bestemming en bewaart de lokale originelen tot succes bevestigd is.
Ontkoppelen waarschuwt voor niet-gesynchroniseerde wijzigingen en biedt export aan.
Een onbereikbare server schakelt een Memos-collectie niet ongemerkt om naar lokale modus.

Lokale opslagvoorstel: SQLite voor notities, mappen, zoekindex en sync-administratie.
Afbeeldingsbestanden lokaal in app-private opslag bewaren, met verwijzingen in SQLite.
Lokale afbeeldingen blijven beschikbaar zonder backend; export neemt de bestanden mee.
Opslag volgt XDG-locaties; in Flatpak de bijbehorende app-private locaties.
Lokale data is niet automatisch versleuteld: schijfversleuteling en eventuele extra
databaseversleuteling moeten afzonderlijk worden beoordeeld.
Export naar Markdown met mapstructuur en metadata hoort bij de eerste release.

## 4. Memos 0.31.0: gecontroleerde API-afspraken

De upstream API-definities van tag v0.31.0 zijn geraadpleegd, niet de echte Umbrel-server.
Een integratieproef tegen de gebruikersinstallatie blijft nodig voor release.

| Onderdeel | API / betekenis |
| --- | --- |
| Gebruiker controleren | `GET /api/v1/auth/me` |
| Notities lezen / maken | `GET /api/v1/memos`, `POST /api/v1/memos` |
| Notitie lezen / wijzigen / verwijderen | `GET`, `PATCH`, `DELETE /api/v1/memos/{id}` |
| Mappen via spaces | `GET`, `POST /api/v1/spaces`; `GET`, `PATCH`, `DELETE /api/v1/spaces/{id}` |
| App key | Personal Access Token (PAT), niet het kortlevende sessietoken |
| Inhoud | Markdown; `property.title` wordt afgeleid uit de eerste H1 |
| Groepering | Optioneel `space`-veld, bijvoorbeeld `spaces/{id}` |
| Tags | Afgeleid uit inhoud en output-only; geen zelfstandig beschrijfbaar folderveld |
| Updates | Field masks: alleen bedoelde velden wijzigen |
| Lijsten | Paginering volledig verwerken; serverfilters en sortering expliciet kiezen |
| Archief | State `ARCHIVED`; niet hetzelfde als een herstelbare prullenbak |

PAT-beheer bestaat onder `/api/v1/users/{user}/personalAccessTokens`.
Memonotes maakt deze sleutel niet zelf aan; de gebruiker doet dat in Memos.
Voorgestelde authenticatie is `Authorization: Bearer <PAT>`; dit expliciet testen
tegen de doelinstallatie, inclusief verkeerde, verlopen en ingetrokken tokens.

Veiligheid:

- Fedora: PAT opslaan via Secret Service/keyring, niet in SQLite, configuratie of logs.
- Als de keyring niet beschikbaar is: duidelijke fout of alleen sessiegebruik, geen plaintext fallback.
- HTTPS als standaard. HTTP voor een lokaal Umbrel-netwerk alleen na expliciete waarschuwing;
  de sleutel en notities zijn daar niet transportversleuteld.
- TLS-certificaatcontrole nooit stilzwijgend uitschakelen; token niet doorsturen naar andere hosts.
- De gebruiker moet de server zelf kunnen bereiken, lokaal of via eigen beveiligde toegang.
  Memonotes maakt Umbrel niet automatisch bereikbaar vanaf internet.
- Rich-text-inhoud en afbeeldingen veilig verwerken; geen uitvoerbare HTML of scripts uit notities toelaten.

## 5. Besluit: een space is een folder

Productbesluit: een Memos-space is een folder met daarin notes, zonder submappen.
Technisch te toetsen: een space met alleen de eigenaar als lid werkt als persoonlijke map.
Lokale mappen krijgen een lokale ID; Memos-mappen verwijzen naar een echte space-resource.
Notities zonder `space` verschijnen in Zonder map. In versie 1 maximaal een map per notitie
en geen geneste mappen: de geraadpleegde Space-definitie biedt geen parent-veld.

- Nieuwe Memonotes-notities standaard `PRIVATE`, ook wanneer ze in een space staan.
- Mapverplaatsing verandert zichtbaarheid niet impliciet.
- Gedeelde spaces en niet-private notities vallen buiten scope en worden niet in Memonotes getoond; de cache bewaart ze alleen voor sync-administratie.
- Uitnodigingen, ledenbeheer en publiceren vallen buiten versie 1.
- Space verwijderen kan volgens de API ook alle geplaatste notities wissen.
  De bevestigingsdialoog biedt daarom drie keuzes: annuleren, de map verwijderen
  en notities behouden, of de map met alle notities verwijderen. Bij behoud worden
  notities eerst naar Zonder map verplaatst; bij verwijderen worden notities eerst
  verwijderd. Pas daarna wordt `FolderDeleted` verwerkt. De core spaceflow is live
  getest; gelijktijdige serverwijzigingen blijven onderdeel van de sync-validatie.
- Discriminerende integratieproef: PAT gebruiken om een eigen space te maken, een PRIVATE
  notitie eraan te koppelen, te hernoemen en vanuit een tweede client te lezen/verplaatsen.
- Bij ongeschikte space-semantiek een alternatief voorstellen en opnieuw afstemmen;
  geen verborgen metadata- of tagconventie introduceren zonder besluit.

## 6. Offline en synchronisatie

De UI leest en schrijft altijd lokaal. In Memos-modus zijn SQLite en een duurzame
outbox de lokale cache en wachtrij; Memos blijft de gedeelde server voor meerdere apparaten.
Notitie opslaan en de bijbehorende wachtrijactie vormen samen een database-transactie.

Voorgesteld minimaal model:

- Account: modus, serveradres, remote gebruikers-ID en verwijzing naar keyring-item.
- Folder: lokale ID, account-ID, naam, optionele remote space-ID en sync-status.
- Note: lokale ID, account-ID, folder-ID, Markdown, pin/state/visibility en remote memo-ID.
- ImageAsset: lokale ID, bestandspad, media-type, optionele remote attachment-ID en upload/cache-status.
- SyncMetadata: laatst gesynchroniseerde serverinhoud, remote update_time, lokale revisie.
- Outbox: duurzame mutaties, doel-ID, payload, retry-status en afhankelijkheden.
- Conflict: basisversie, lokale versie en remote versie voor herstel en vergelijking.

Sync verloopt bij verbinden, terugkeer online, handmatig verversen en periodiek zolang
de app draait. Netwerkwerk blokkeert nooit de GTK-mainloop.
Paginering, backoff, authenticatiefouten en map-aanmaak voor notitie-aanmaak horen erbij.
Afbeeldingen offline invoegen slaat het bestand lokaal op en maakt een duurzame uploadactie.
Bij sync eerst de benodigde memo/attachment-resources aanmaken volgens de geteste Memos-API,
daarna de definitieve afbeeldingsverwijzing publiceren; geen lokale bestandspaden naar de server sturen.
Uploadretries en onderbroken uploads mogen geen dubbele of verweesde attachments opleveren.
Gekoppelde afbeeldingen lokaal cachen voor offline lezen; ontbrekende bestanden zichtbaar melden.
De attachment-API, limieten en URL/authenticatie moeten nog apart worden onderzocht en getest.
Retries mogen geen dubbele notities maken: stabiele API-compatibele IDs gebruiken en
na een onzekere create-uitkomst eerst de serverstatus opvragen. Dit is geïmplementeerd
voor note creates; attachment-retries moeten dit later ook volgen.
Een wijziging tijdens een upload wordt niet door de uploadbevestiging als gesynchroniseerd gemarkeerd.

Conflicten niet oplossen met blind last-write-wins: basis, lokaal en remote vergelijken.
De app bewaart die drie versies en laat de gebruiker lokaal of server behouden.
Automatisch samenvoegen bestaat niet.
De bekeken API-definities bewijzen geen atomische compare-and-swap; een read-before-write
is dus geen volledige racebescherming. Dit onderzoeken in de integratieproef en expliciet
testen met meerdere desktopclients en de Memos-webclient.
Ontbrekende items in een gedeeltelijke lijst betekenen nooit automatisch verwijdering.
Remote deletions worden alleen na directe GET-confirmatie verwerkt. Deletions met
pending lokale edits of een gelijktijdige serveredit leiden tot herstel/conflict,
niet stil dataverlies.
De huidige app biedt archiveren en harde verwijdering vanuit het archief, met een
undo-stack in het geheugen. `NoteRestored` maakt een verwijderde snapshot opnieuw
aan; attachments, relaties en shares worden niet hersteld. Een robuust ontwerp
voor verwijdering/herstel en een eventuele prullenbak blijft nodig.

## 7. Linux/GNOME-architectuur

Desktoplagen: GTK/libadwaita UI -> Rust domeinlogica -> SQLite repository en Memos API-adapter.
Geschikte bouwstenen evalueren: gtk-rs, libadwaita-rs, rusqlite, reqwest, serde,
Secret Service-integratie, een bewezen rich-text-engine en Markdown-conversie.
Geen eigen Markdown-parser schrijven; onbekende Memos-inhoud bij bewerken niet verliezen.

Het editorprototype bepaalt of een geschikte native editorintegratie volstaat of dat een
ingebedde editor nodig is. Een ingebedde oplossing verandert de Rust/GTK-appshell niet,
maar vraagt expliciete validatie van GNOME-integratie, focus, toegankelijkheid, veilige
inhoudsverwerking en Flatpak-verpakking voordat die gekozen wordt.
Geen cross-platform UI-framework, mobiele bindings of gedeelde React Native-core in scope.
Focus op GNOME-vensterbediening, systeembestandkiezer, keyring, toetsenbord en Wayland.
Memos-collecties kunnen via de server met andere clients synchroniseren; lokale-only collecties niet.

## 8. Gefaseerde uitvoering en acceptatie

1. API-proef: PAT, huidige gebruiker, CRUD, space-mapping, PRIVATE-gedrag, update masks,
  paginering, archive, attachments en concurrent updates controleren op Memos 0.31.0.
2. UI/editor-prototype: drie panelen, selectie, zoeken, WYSIWYG en adaptieve navigatie.
  Checkboxes, bold/italic en inline afbeeldingen werkend tonen; roundtrip en
  GNOME-integratie valideren voordat de editor-engine definitief wordt gekozen.
3. Lokale MVP: SQLite, autosave, platte mappen, verplaatsen, pinnen, archief en export met afbeeldingen.
   Starten zonder backend doet geen netwerkverzoeken; herstart en crash verliezen geen bevestigde saves.
4. Memos-integratie: server/PAT-onboarding, keyring, ophalen en duurzame outbox.
  Zonder netwerk tekst en afbeeldingen lezen/bewerken; na reconnect sync zonder duplicaten of stil verlies.
5. Robuustheid en Fedora-release: sync-conflicten, tokenfouten, migraties, packaging.
   Voorstel distributie: Flatpak; RPM pas bij behoefte. Netwerk en keyringtoegang in sandbox testen.

Testdekking richt zich op opslagtransacties, herstart met pending outbox, verloren
responses, retries, gelijktijdige edits, remote deletions en accountwissel.
Editorchecks omvatten opmaak/checklist-roundtrips, afbeeldingen
na herstart, offline afbeeldingsuploads, undo/redo en behoud van onbekende Memos-inhoud.
UI-checks omvatten leegte/fout/offline-staten, paneelresizing, toetsenbordbediening en toegankelijkheid.

## 9. Nog samen te besluiten

- Een optionele Markdown-brontab toevoegen of uitsluitend WYSIWYG aanbieden?
- Hoe dicht moet de vormgeving bij Apple Notes komen versus standaard libadwaita?
- Naast bevestigde afbeeldingen ook andere bestandsbijlagen in versie 1?
- Meerdere gekoppelde accounts, zoals nu gebouwd, definitief opnemen in versie 1?

Technische proeven, geen nog open productkeuzes: editor-engine,
persoonlijke space-semantiek en veilige offline afbeeldingssync. Mobiel is buiten scope.

## 10. Bronnen en beperkingen

- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/memo_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/auth_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/space_service.proto
- https://raw.githubusercontent.com/usememos/memos/v0.31.0/proto/api/v1/user_service.proto

De app en lokale opslag/API-mocktests bestaan inmiddels in deze workspace.
De huidige code is leidend voor de gerealiseerde checklist; de overige secties
blijven productdoelen en acceptatiecriteria. Core live servercompatibiliteit is
bevestigd; concurrente updates, attachment-gedrag en release/sandboxtests zijn
nog niet gevalideerd.