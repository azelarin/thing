--!strict
local checked=ui.state(false)
return ui.panel {
    accessibility={role='toolbar',label='media controls'},
    children={
        ui.button {
            accessibility={role='switch',label=function() return if checked:get() then 'unmute' else 'mute' end,
                description='microphone',checked=function() return checked:get() end},
            on_click=function() checked:set(not checked:get()) end,
        },
        ui.button {accessibility={role='checkbox',checked='mixed'}},
        ui.text {text='ready',accessibility={role='status',live='polite',description=function(): string? return nil end}},
        ui.slider {accessibility={value_text=function() return '42 percent' end}},
    },
}
