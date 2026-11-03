--!strict
services.watch_file { key='data', path=true, on_change=function() end }
services.watch_file { key='data', path='./data.txt', on_change=true }
return ui.panel {}
