# Home Assistant

The receiver's web panel (`http://10.21.0.232:8445/`) has a small HTTP interface. Home Assistant
(or anything else that can send an HTTP request, such as a wall tablet or a script) can use it
to switch the projector on and off, choose the input, and use hide and freeze. Home Assistant
never talks to the projector itself: the receiver on the VM does that, the same way the panel does.

```
wall tablet / HA dashboard ──► Home Assistant ──HTTP──► receiver VM :8445 ──► projector (Acer web page,
                                                         (acermiracast)          Wake-on-LAN via VM and box)
```

Home Assistant only has to reach TCP 8445 on the VM. It does not need access to the projector's
subnet or to the beacon box.

## The interface

### Reading the state: `GET /api/state`

```json
{"power": "on", "input": "hdmi1", "input_label": "HDMI 1", "hide": false, "freeze": false, "presenter": "YOGA"}
```

| Field | Values | Meaning |
|---|---|---|
| `power` | `on`, `off`, `warming`, `cooling`, `unknown` | `warming`: switched on, the projector is starting and not answering yet (at most 90 s, then back to `off` if it never answered). `cooling`: switched off, the fan runs; the projector does not take commands during this time. `unknown`: the projector answered without a state. |
| `input` | `hdmi1`, `hdmi2`, `wireless`, `vga1`, `vga2`, `video`, `media`, `usb`, `null` | Current input. `null` while the projector is not `on`. `wireless` is the LAN/WiFi input that Win+K uses. |
| `input_label` | text | The input's name as the panel shows it (`HDMI 1`, `Drahtlos`, …). |
| `hide` | `true` / `false` | Picture hidden (blank). Always `false` while the projector is not `on`. |
| `freeze` | `true` / `false` | Picture frozen. Always `false` while the projector is not `on`. |
| `presenter` | name or `null` | Device that is projecting with Win+K right now (`null` = nobody). |

The receiver asks the projector every 5 s and right after every command. `/api/state` answers
from what it already knows, so it is cheap: polling every 5-10 s from Home Assistant is fine and
does not put load on the projector.

`GET /api/status` gives everything the panel shows (much larger). Use `/api/state` for
Home Assistant.

### Commands: `POST /api/projector`

The body is JSON, and the header `Content-Type: application/json` is required:

```json
{"action": "hide", "value": "on"}
```

| `action` | `value` | What happens |
|---|---|---|
| `power` | `on` | Projector off: the VM sends Wake-on-LAN, and the box sends it too from inside the projector's subnet (`wake_url`). The state is `warming` until the projector answers (about 30-60 s); sending `on` again meanwhile only repeats the wake packet. Projector already `on` or `cooling`: nothing is sent. |
| `power` | `off` | Switches off (the projector's power button). Nothing happens if it is `cooling` or in standby. While `warming` the projector is not on the network yet and the answer is `Der Beamer ist aus`. |
| `power` | `toggle` | `off` when it is `on`, otherwise `on`. |
| `input` | `hdmi1`, `hdmi2`, `wireless`, `vga1`, `vga2`, `video`, `media`, `usb` | Switches the input. The projector has to be `on`. The projector's own source codes (`3`, `6`, `33`, …) work as well. |
| `hide` | `on`, `off`, `toggle` | Hides / shows the picture. The projector's button can only toggle, so for `on` and `off` the receiver asks the projector first and presses only if the state differs. Sending `on` twice therefore keeps the picture hidden. |
| `freeze` | `on`, `off`, `toggle` | Freezes / releases the picture, like `hide`. |
| `p2p_mode` | `only`, `fallback`, `off` | Wi-Fi Direct mode of the beacon box (see the panel's "Funk-Sender" card and the README). Refused while somebody projects. |

`on`/`off` can also be written as `1`/`0` or `true`/`false`. Every other action of the panel
(picture settings, remote keys, administration) goes through the same address too. The panel's
code (`rust/panel.html`) shows those actions.

Answers:

| HTTP status | Body | When |
|---|---|---|
| 200 | `{"ok": true, "error": null, "data": null}` | Done (or nothing to do) |
| 502 | `{"ok": false, "error": "Der Beamer ist aus"}` | The command could not be carried out; `error` says why (German, as in the panel) |
| 400 | `{"ok": false, "error": "Ungültige Anfrage"}` | Body is not JSON or has no `action` |
| 415 | `{"ok": false, "error": "Content-Type muss application/json sein"}` | Header `Content-Type: application/json` missing |

Error texts you may see:

| `error` | Meaning |
|---|---|
| `Der Beamer ist aus` | The projector is switched off or still starting (not on the network); only `power on` works now |
| `Der Beamer ist nicht eingeschaltet` | `hide`/`freeze`/`input` while `warming`, `cooling` or in standby |
| `Der Beamer antwortet nicht` | The projector's web page did not answer (busy, or it just went off) |
| `Wert muss on, off oder toggle sein` | Wrong `value` for `hide`/`freeze` |
| `Unbekannter Befehl` | Unknown `action` or input name |
| `Einschalten ist nicht eingerichtet …` | Neither `projector_mac` nor `wake_url` is in the config |

### Trying it by hand

```bash
curl http://10.21.0.232:8445/api/state
curl -X POST -H 'Content-Type: application/json' -d '{"action":"freeze","value":"toggle"}' http://10.21.0.232:8445/api/projector
```

PowerShell:

```powershell
Invoke-RestMethod http://10.21.0.232:8445/api/state
Invoke-RestMethod http://10.21.0.232:8445/api/projector -Method Post -ContentType application/json -Body '{"action":"input","value":"hdmi1"}'
```

### Security

There is no login, as with the panel. Anybody who can reach TCP 8445 on the VM can control the
projector. The `Content-Type: application/json` requirement stops other web pages: a browser cannot
send such a request to another address without asking first (CORS preflight), and the receiver never
says yes to that. To allow only Home Assistant and the hub, limit port 8445 in `ufw` on the VM to their
addresses.

## Home Assistant configuration

Everything below goes into `configuration.yaml` (or a package file). It needs Home Assistant
2025.2 or newer (modern template switches, `action:` instead of `service:`). The syntax has not been
tried against a running Home Assistant here; check it with Developer tools -> YAML -> Check
configuration, then restart.

Quote `"on"` and `"off"` everywhere: unquoted, YAML reads them as true/false, and the receiver
would get `True`.

### 1. One command for everything

```yaml
rest_command:
  beamer:
    url: http://10.21.0.232:8445/api/projector
    method: POST
    content_type: application/json
    payload: '{"action": "{{ action }}", "value": "{{ value }}"}'
    timeout: 15
```

The action `rest_command.beamer` with `data: {action: hide, value: "on"}` is then all you need
for buttons, scripts and automations.

### 2. Sensors from the state

```yaml
rest:
  - resource: http://10.21.0.232:8445/api/state
    scan_interval: 10
    timeout: 5
    sensor:
      - name: Beamer Power
        unique_id: beamer_power
        icon: mdi:projector
        value_template: "{{ value_json.power }}"
      - name: Beamer Input
        unique_id: beamer_input
        icon: mdi:video-input-hdmi
        value_template: "{{ value_json.input or 'none' }}"
      - name: Beamer Presenter
        unique_id: beamer_presenter
        icon: mdi:laptop
        value_template: "{{ value_json.presenter or 'niemand' }}"
    binary_sensor:
      - name: Beamer Hide
        unique_id: beamer_hide_state
        value_template: "{{ value_json.hide }}"
      - name: Beamer Freeze
        unique_id: beamer_freeze_state
        value_template: "{{ value_json.freeze }}"
```

This makes `sensor.beamer_power`, `sensor.beamer_input`, `sensor.beamer_presenter`,
`binary_sensor.beamer_hide` and `binary_sensor.beamer_freeze`. If the VM is not reachable, they
become `unavailable`.

### 3. Switches and the input selection

The switches send the command and then fetch the state at once, so the tablet shows the result
without waiting for the next poll. `hide` and `freeze` report the new state immediately; power and
input are read back by the receiver within a second or two, hence the short delay.

```yaml
template:
  - switch:
      - name: Beamer
        unique_id: beamer_power_switch
        icon: mdi:projector
        state: "{{ states('sensor.beamer_power') in ['on', 'warming'] }}"
        turn_on:
          - action: rest_command.beamer
            data: {action: power, value: "on"}
          - delay: 2
          - action: homeassistant.update_entity
            target: {entity_id: sensor.beamer_power}
        turn_off:
          - action: rest_command.beamer
            data: {action: power, value: "off"}
          - delay: 2
          - action: homeassistant.update_entity
            target: {entity_id: sensor.beamer_power}

      - name: Beamer Hide
        unique_id: beamer_hide_switch
        icon: mdi:projector-off
        state: "{{ is_state('binary_sensor.beamer_hide', 'on') }}"
        availability: "{{ is_state('sensor.beamer_power', 'on') }}"
        turn_on:
          - action: rest_command.beamer
            data: {action: hide, value: "on"}
          - action: homeassistant.update_entity
            target: {entity_id: binary_sensor.beamer_hide}
        turn_off:
          - action: rest_command.beamer
            data: {action: hide, value: "off"}
          - action: homeassistant.update_entity
            target: {entity_id: binary_sensor.beamer_hide}

      - name: Beamer Freeze
        unique_id: beamer_freeze_switch
        icon: mdi:pause-box
        state: "{{ is_state('binary_sensor.beamer_freeze', 'on') }}"
        availability: "{{ is_state('sensor.beamer_power', 'on') }}"
        turn_on:
          - action: rest_command.beamer
            data: {action: freeze, value: "on"}
          - action: homeassistant.update_entity
            target: {entity_id: binary_sensor.beamer_freeze}
        turn_off:
          - action: rest_command.beamer
            data: {action: freeze, value: "off"}
          - action: homeassistant.update_entity
            target: {entity_id: binary_sensor.beamer_freeze}

  - select:
      - name: Beamer Input
        unique_id: beamer_input_select
        icon: mdi:video-input-hdmi
        state: "{{ states('sensor.beamer_input') }}"
        availability: "{{ is_state('sensor.beamer_power', 'on') }}"
        options: "{{ ['hdmi1', 'hdmi2', 'wireless', 'vga1', 'vga2', 'video', 'media', 'usb'] }}"
        select_option:
          - action: rest_command.beamer
            data: {action: input, value: "{{ option }}"}
          - delay: 2
          - action: homeassistant.update_entity
            target: {entity_id: sensor.beamer_input}
```

Result: `switch.beamer`, `switch.beamer_hide`, `switch.beamer_freeze`, `select.beamer_input`.
Hide, freeze and the input selection are greyed out (unavailable) while the projector is not on,
because the projector refuses them then.

Inputs that are not connected can be left out of `options`. Win+K needs `wireless`.

### 4. Wall tablet dashboard

A card for the dashboard (edit dashboard -> add card -> manual):

```yaml
type: vertical-stack
cards:
  - type: grid
    columns: 3
    square: false
    cards:
      - type: button
        entity: switch.beamer
        name: Beamer
        show_state: true
      - type: button
        entity: switch.beamer_hide
        name: Hide
        show_state: true
      - type: button
        entity: switch.beamer_freeze
        name: Freeze
        show_state: true
  - type: grid
    columns: 3
    square: false
    cards:
      - type: button
        name: HDMI 1
        icon: mdi:video-input-hdmi
        tap_action:
          action: perform-action
          perform_action: rest_command.beamer
          data: {action: input, value: hdmi1}
      - type: button
        name: HDMI 2
        icon: mdi:video-input-hdmi
        tap_action:
          action: perform-action
          perform_action: rest_command.beamer
          data: {action: input, value: hdmi2}
      - type: button
        name: Win+K
        icon: mdi:cast
        tap_action:
          action: perform-action
          perform_action: rest_command.beamer
          data: {action: input, value: wireless}
  - type: entities
    entities:
      - entity: sensor.beamer_power
        name: Zustand
      - entity: sensor.beamer_presenter
        name: Zeigt gerade
```

A switch button toggles on tap. An input button only sends the command; the current input is in
`sensor.beamer_input` (or use the `select.beamer_input` entity in an entities card instead of the
buttons).

### 5. Automations (examples)

Switch off at night when nobody projects:

```yaml
automation:
  - alias: Beamer abends aus
    triggers:
      - trigger: time
        at: "22:30:00"
    conditions:
      - condition: state
        entity_id: sensor.beamer_power
        state: "on"
      - condition: state
        entity_id: sensor.beamer_presenter
        state: niemand
    actions:
      - action: rest_command.beamer
        data: {action: power, value: "off"}
```

Release freeze and hide when a new laptop starts projecting (so nobody wonders why their screen
does not show):

```yaml
  - alias: Beamer neuer Vortrag
    triggers:
      - trigger: state
        entity_id: sensor.beamer_presenter
        from: niemand
    actions:
      - action: rest_command.beamer
        data: {action: freeze, value: "off"}
      - action: rest_command.beamer
        data: {action: hide, value: "off"}
```

## Troubleshooting

- **HA log shows status 415**: `content_type: application/json` is missing in `rest_command`, or
  `Content-Type` in a hand-made request.
- **Status 502**: the command was refused; the reason is in the answer's `error`. The receiver's
  log has it too: `sudo journalctl -u acermiracast | grep panel:`.
- **Sensors `unavailable`**: Home Assistant cannot reach `10.21.0.232:8445` (firewall, VM off).
  Test with `curl http://10.21.0.232:8445/api/state` from the HA host.
- **Switching on takes long**: normal. The projector is off the network when switched off, and only
  the Wake-on-LAN packet brings it back. The state stays `warming` until it answers.
- **Switch flips back**: the projector refused the command (for example, `power on` while `cooling`).
  Wait until it is `off`, then switch on again.
- **The value arrives as `True`**: an unquoted `on` in YAML. Write `"on"`.
