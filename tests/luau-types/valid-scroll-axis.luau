--!strict

local horizontal = ui.state(true)
return ui.panel {
    children = {
        ui.scroll { axis = "horizontal", style = { width = 240, height = 64 }, children = {
            ui.text { text = "first" }, ui.text { text = "second" },
        } },
        ui.scroll { axis = function(): ThingScrollAxis
            return if horizontal:get() then "horizontal" else "vertical"
        end },
        ui.scroll {},
    },
}
