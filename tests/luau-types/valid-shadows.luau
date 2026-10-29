--!strict

local raised = ui.state(false)
local card: ThingShadow = { color = '#00000080', blur = 12, offset_y = 4 }
local hard: ThingShadow = { color = '#000000', spread = -2, offset_x = 3, offset_y = 3 }

return ui.panel {
    children = {
        ui.button {
            on_click = function() raised:set(not raised:get()) end,
            style = {
                radius = 8,
                box_shadow = function()
                    return if raised:get() then card else nil
                end,
            },
            children = {
                ui.path {
                    data = 'M 12 2 L 22 20 H 2 Z',
                    view_box = { x = 0, y = 0, width = 24, height = 24 },
                    shadow = hard,
                },
            },
        },
        ui.text { text = 'label', style = { box_shadow = { color = '#11111b' } } },
        ui.text_input {
            value = '',
            on_change = function(_value: string) end,
            style = { box_shadow = nil },
        },
    },
}
