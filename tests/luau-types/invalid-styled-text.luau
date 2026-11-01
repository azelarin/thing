--!strict

return ui.panel {
    children = {
        ui.text { spans = { { text = 42 } } },
        ui.text { markup = true },
        ui.text { text = "x", on_link = function(target: number) end },
        ui.text { text = "x", style = { letter_spacing = "wide" } },
    },
}
