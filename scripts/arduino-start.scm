(require-builtin steel/time)

;; Server
(define server "127.0.0.1:7240")

;; Global parameter keys
(define heater "furnace1/heater_temperature")
(define sample "furnace1/sample_temperature")
(define room "furnace1/room_temperature")
(define power "furnace1/power")

;; Short aliases
(define status lab/status)
(define get lab/get)
(define set lab/set!)

(define (power! value)
  (set power value))

(define (sub! key)
  (lab/subscribe! (list key)))

(define (unsub! key)
  (lab/unsubscribe! (list key)))

(define (sub-all!)
  (lab/subscribe! (list heater sample room power)))

(define (curve! name unit key)
  (plot/add! name unit)
  (plot/bind! name key))

;; Wait up to 10 seconds for connection
(define (wait-ready! attempts)
  (cond
    ((equal? (lab/status) "Ready") #t)
    ((equal? (lab/status) "Stopped") #f)
    ((<= attempts 0) #f)
    (else
     (time/sleep-ms 100)
     (wait-ready! (- attempts 1)))))

;; Connect only when needed
(define (connect!)
  (cond
    ((equal? (lab/status) "Ready") #t)
    ((equal? (lab/status) "Connecting")
     (wait-ready! 100))
    ((equal? (lab/status) "Stopped")
     (and (lab/connect! server)
          (wait-ready! 100)))
    (else #f)))

;; Configure randomized thermal model
(define (configure-model!)
  (set "furnace1/designer/seed" 12345)
  (set "furnace1/designer/parameter.spread" 0.03)
  (set "furnace1/designer/power.fluctuation" 0.03)
  (set "furnace1/designer/sensor.noise" 0.2)
  (set "furnace1/designer/sensor.drift" 0.1)
  (lab/invoke-action! "furnace1/apply"))

;; Initialize plots and subscriptions
(define (start!)
  (if (connect!)
      (begin
        (configure-model!)
        (curve! "Heater" "degC" heater)
        (curve! "Sample" "degC" sample)
        (curve! "Room" "degC" room)
        (curve! "Power" "%" power)
        (sub-all!))
      (error "Connection to lab-server did not become Ready")))

(start!)
