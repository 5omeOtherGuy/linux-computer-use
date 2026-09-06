import asyncio
import base64
import io
import json
import time

import pytest
from PIL import Image
from Xlib import X, display

from linux_computer_use.errors import DesktopError
from linux_computer_use.supervisor import Supervisor

pytestmark = pytest.mark.desktop


async def execute(supervisor, code, error=None, timeout=15):
    response = await supervisor.request("exec", {"code": code}, timeout)
    if error:
        assert response["isError"], response
        assert error in str(response["content"]), response
    else:
        assert not response["isError"], response
    return response


async def state_until(path, predicate, timeout=3):
    deadline = time.monotonic()+timeout
    while True:
        state = json.loads((path / "state.json").read_text())
        if predicate(state):
            return state
        if time.monotonic() > deadline:
            raise AssertionError(state)
        await asyncio.sleep(.02)


def test_named_controls_unicode_and_persistence(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); field=w.find(name='Message',role='text')")
            expected = "Accessibility: Grüße Ελληνικά 漢字 🙂"
            await execute(s, f"field.set_value({expected!r}); w.find(name='Enable option').click(); w.find(name='Save message').click(); w.wait_for(name='Saved successfully')")
            state = await state_until(desktop_fixture, lambda d: d['saved'] == expected)
            assert state['checked']
            r = await execute(s, "w.observe()")
            image = next(c for c in r['content'] if c['type'] == 'image')
            assert Image.open(io.BytesIO(base64.b64decode(image['data']))).size == (760,640)
        finally:
            await s.close()
    asyncio.run(run())


def test_real_keyboard_unicode_and_clipboard_independence(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); w.observe(screenshot=False); b=w.find(name='Message').data['bounds']; g=w.info(); w.click(b['x']-g['x']+20,b['y']-g['y']+15)")
            expected = "Native keys: Abc 123! äöü € 漢字 🙂"
            # Repeat to expose asynchronous keyboard-map races, not just a lucky run.
            for index in range(10):
                value = f"{expected} {index}"
                await execute(s, f"w.press_key('Ctrl+a'); w.type_text({value!r}); w.wait_for(name='Save message').click()")
                await state_until(desktop_fixture, lambda d: d['saved'] == value)
            # Selection uses the real AT-SPI text interface.
            await execute(s, "w.find(name='Message').select_text('Native keys')")
        finally:
            await s.close()
    asyncio.run(run())


def test_canvas_drag_scroll_and_native_fallback(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); w.observe(screenshot=False); b=w.find(name='Drag canvas').data['bounds']; g=w.info(); cx=b['x']-g['x']; cy=b['y']-g['y']; w.drag([[cx+60,cy+90],[cx+300,cy+90],[cx+610,cy+90]]); w.scroll('down',steps=3,x=cx+300,y=cy+90)")
            state = await state_until(desktop_fixture, lambda d: d['drop'] and d['scroll'] >= 3)
            assert state['drop']
            await execute(s, "desktop.observe(screenshot=False); p=desktop.x.pointer()\nif desktop.status()['capabilities']['native_sky']:\n desktop.native('move',x=p['x']+1,y=p['y']); assert desktop.x.pointer()['x']==p['x']+1; desktop.native('move',x=p['x'],y=p['y'])")
        finally:
            await s.close()
    asyncio.run(run())


def test_menu_and_native_file_dialog(desktop_fixture):
    selected = desktop_fixture / 'example.txt'
    selected.write_text('Local file chooser acceptance\n')
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); w.find(name='Options').click(); w.wait_for(name='Apply preset').click(); w.wait_for(name='Message',text='Preset applied'); w.find(name='Open file dialog').click()")
            deadline = time.monotonic()+5
            while True:
                r = await s.request('status')
                if any(w['title']=='LCU File Chooser' for w in r['structuredContent']['observation']['windows']):
                    break
                assert time.monotonic() < deadline
                await asyncio.sleep(.05)
            await execute(s, f"dialog=desktop.window('LCU File Chooser'); dialog.observe(screenshot=False); dialog.press_key('Ctrl+l'); dialog.type_text({str(selected)!r}); dialog.press_key('Return')")
            await state_until(desktop_fixture, lambda d: d['selected_file'] == str(selected))
        finally:
            await s.close()
    asyncio.run(run())


def test_stale_ambiguous_disabled_and_changed_geometry(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); old=w.find(name='Message'); w.find(name='Replace field').click(); w.wait_for(name='Message')")
            await execute(s, "old.set_value('must not be written')", 'STALE_ELEMENT')
            await execute(s, "w.find(name='Duplicate').click()", 'AMBIGUOUS_ELEMENT')
            await execute(s, "w.find(name='Disabled action').click()", 'NOT_ACTIONABLE')
            await execute(s, "w.click(10,10)", 'OBSERVATION_REQUIRED')
            await execute(s, "w.observe(screenshot=False); original=w.info(); desktop.x.configure(w.id,x=original['x']+40)")
            await execute(s, "w.click(10,10)", 'GEOMETRY_CHANGED')
            await execute(s, "w.observe(screenshot=False); w.find(name='Message').set_value('Recovered'); w.find(name='Save message').click()")
            await state_until(desktop_fixture, lambda d: d['saved'] == 'Recovered')
        finally:
            await s.close()
    asyncio.run(run())


def test_timeout_releases_input_and_allows_new_runtime(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); w.observe(screenshot=False)")
            with pytest.raises(asyncio.TimeoutError):
                await s.request('exec', {'code': "desktop.x.button_down(); desktop.x._key_down(desktop.x.keycode('Shift_L')); import time; time.sleep(20)"}, timeout=.2)
            connection = display.Display()
            assert not connection.screen().root.query_pointer().mask & X.Button1Mask
            shift = connection.keysym_to_keycode(0xffe1)
            assert not connection.query_keymap()[shift//8] & (1 << (shift%8))
            connection.close()
            await execute(s, "print(desktop.status()['session'])")
        finally:
            await s.close()
    asyncio.run(run())


def test_cancel_and_cross_client_input_lease(desktop_fixture):
    async def run():
        first, second = Supervisor(), Supervisor()
        task = None
        try:
            await first.request('status')
            task = asyncio.create_task(first.request('exec', {'code': 'import time; desktop.x.button_down(); time.sleep(20)'}))
            for _ in range(100):
                if first.ownership.exists() and json.loads(first.ownership.read_text()).get('buttons'):
                    break
                await asyncio.sleep(.02)
            with pytest.raises(DesktopError, match='Another local client'):
                await second.request('status')
            stopped = await first.stop()
            assert stopped['input_released']
            with pytest.raises(DesktopError):
                await task
            await second.request('status')
        finally:
            await first.close()
            await second.close()
            if task and not task.done():
                task.cancel()
    asyncio.run(run())


def test_global_input_and_observation_diffs(desktop_fixture):
    async def run():
        s = Supervisor()
        try:
            await execute(s, "desktop.click(1,1)", 'OBSERVATION_REQUIRED')
            await execute(s, "w=desktop.window('Linux Computer Use Acceptance'); first=w.observe(); assert first['accessibility']['mode']=='full'; ids={e['name']:e['id'] for e in first['accessibility']['elements']}; b=w.find(name='Message').data['bounds']; desktop.observe(screenshot=False); desktop.click(b['x']+20,b['y']+15); desktop.press_key('Ctrl+a'); desktop.type_text('Global input verified'); w.find(name='Save message').click(); w.wait_for(name='Saved successfully')")
            await state_until(desktop_fixture, lambda d: d['saved']=='Global input verified')
            await execute(s, "changed=w.observe(screenshot=False); assert changed['accessibility']['mode']=='diff'; field=next(e for e in changed['accessibility']['elements'] if e['name']=='Message'); assert field['id']==ids['Message']; assert field['text']=='Global input verified'; full=w.observe(screenshot=False,diff=False); assert full['accessibility']['mode']=='full'")
            await execute(s, "w.find(name='Message',max_elements=1)", 'SEARCH_TRUNCATED')
        finally:
            await s.close()
    asyncio.run(run())
