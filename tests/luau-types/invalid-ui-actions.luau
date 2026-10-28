--!strict

local button = ui.button {
    autofocus = "yes",
}

ui.focus("button")
ui.reveal(true)

return ui.panel { children = { button } }
